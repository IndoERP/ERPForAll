// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hand-written CRUD acceptance manifest and runner for the multi-user HTTP suite.
//! Expected outcomes are authored here from the product, uom, base and security
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
    any::Any,
    cell::{Cell, OnceCell},
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
};

/// The five RPC operations of every cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// The principal's own record (its company's, or the one it owns) and the shared record when
    /// the model seeds one, and nothing else.
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
    /// 403 AccessError naming the field the principal may not write.
    FieldDenied(&'static str),
    /// 401 for an anonymous call.
    Unauthenticated,
    /// 404 MissingError: the caller may write, and the identity names no record.
    Missing,
    /// HTTP 200 with an empty list: the caller may read, and the identity names no record (a read
    /// skips records that do not exist).
    NoRecord,
    /// 403 AccessError carrying the attachment refusal (another user's standalone attachment).
    DocumentDenied,
    /// 403 AccessError: the values link a user record the caller cannot read.
    UnreadableUser,
    /// The caller holds the grant, and the model itself refuses the operation (an immutable or
    /// service-managed model): the exact status, error name and message.
    Refused(&'static Guard),
    /// Not yet written; names the board task that will write it.
    Deferred(&'static str),
}

/// A refusal a model raises for a caller who passed the access check.
#[derive(PartialEq, Eq, Debug)]
pub struct Guard {
    pub op: Op,
    pub status: u16,
    pub name: &'static str,
    pub message: &'static str,
    /// The model refuses before it checks the caller's access, whatever the records: every
    /// caller gets this refusal, also one without any grant. Allowed only for a refusal that
    /// does not depend on the records, so it tells the caller nothing.
    pub before_access: bool,
}

/// Who holds the grant for one operation of a base model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// No access row grants it: refused for everyone, the administrator included.
    Nobody,
    /// Settings administrators only.
    Admin,
    /// Every internal user.
    Internal,
}

/// The grant of each operation; search goes with read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grants {
    pub read: Level,
    pub create: Level,
    pub write: Level,
    pub unlink: Level,
}

impl Grants {
    const fn of(self, op: Op) -> Level {
        match op {
            Op::Search | Op::Read => self.read,
            Op::Create => self.create,
            Op::Write => self.write,
            Op::Unlink => self.unlink,
        }
    }
}

/// Where the records of a base model's cells come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rows {
    /// The provisioning administrator creates them through JSON-2.
    Created,
    /// Nobody creates one through RPC: the cells use a row the install ships. A model that ships
    /// none fails the run.
    Shipped,
    /// No row ships and none can be created through RPC. Cells that need an existing record use
    /// an identity no fixture creates: a refused caller gets the model-level denial, a caller who
    /// may read gets an empty read.
    Absent,
    /// Records come and go with a background service (the job queue), so the run claims nothing
    /// about them: the cells work as for `Absent`, without the check that no record exists.
    Volatile,
}

/// How a model's records relate to companies; decides what is seeded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// One record, visible to everyone who may read the model.
    Global,
    /// One record in company A, one in company B and one with no company.
    Company,
    /// One record created by each subject (administrator, A, B), plus a shared one when the model
    /// has shared records.
    Owner,
    /// Records the fixture owns, one per ordinary principal and no shared one: the companies A and
    /// B, the users A and B, or their settings rows.
    Fixture,
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
    /// A base model whose access is a plain table of grants (no record rule): every operation is
    /// either granted to a level or refused at model level, the same for every record.
    Base { grants: Grants, rows: Rows },
    /// Records owned by a user (ir.filters): every internal user, the administrator included,
    /// creates, reads, changes and deletes its own records, reads the shared ones, and is refused
    /// at record level on another user's records and on changing a shared one.
    OwnRecords,
    /// Raw attachments (no owner record): the creator and Settings administrators read, change
    /// and delete one; any other user is refused at record level.
    RawAttachment,
    /// `res.company`: internal users read the companies they belong to and nothing else;
    /// only the managers create, write and unlink.
    OwnCompany { groups: &'static [&'static str] },
    /// `res.users`: internal users read their own record and write its self-service fields;
    /// only the managers create and unlink, or touch another user.
    OwnUser { groups: &'static [&'static str] },
    /// `res.users.settings`: internal users read and write their own row and never another user's;
    /// the owner field is the managers' alone, so a generic create that names an owner
    /// is refused at field level. Unlink of the own row succeeds: the shipped grant
    /// covers it, as the reference's does, while names read and write only (finding
    /// MU-R01 of the report, for the lead to settle).
    OwnSettings { groups: &'static [&'static str] },
    /// Matrix not written yet.
    Deferred(&'static str),
    /// A neighboring test exercises this model's service or model-specific contract.
    Specialized,
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
    /// Refusals the model raises after the access check (immutable or service-managed models).
    pub guards: &'static [Guard],
}

impl ModelCase {
    /// The groups a model-level denial of `op` names, in the order of the message.
    fn groups(&self, op: Op) -> &'static [&'static str] {
        match self.policy {
            Policy::InternalReadOnly { groups }
            | Policy::CompanyScoped { groups, .. }
            | Policy::OwnCompany { groups }
            | Policy::OwnUser { groups }
            | Policy::OwnSettings { groups } => groups,
            Policy::Base { grants, .. } => match grants.of(op) {
                Level::Nobody => &[],
                Level::Admin => SETTINGS,
                // an internal-level grant never refuses a test subject at model level
                Level::Internal => &[],
            },
            Policy::OwnRecords
            | Policy::RawAttachment
            | Policy::Deferred(_)
            | Policy::Specialized => &[],
        }
    }

    /// Where the records of the cells come from.
    fn rows(&self) -> Rows {
        match self.policy {
            Policy::Base { rows, .. } => rows,
            Policy::InternalReadOnly { .. }
            | Policy::CompanyScoped { .. }
            | Policy::OwnRecords
            | Policy::RawAttachment
            | Policy::OwnCompany { .. }
            | Policy::OwnUser { .. }
            | Policy::OwnSettings { .. }
            | Policy::Deferred(_)
            | Policy::Specialized => Rows::Created,
        }
    }

    /// Whether an ordinary principal's write and successful unlink act on its own seeded record
    /// instead of a disposable one.
    fn acts_on_own_record(&self) -> bool {
        matches!(
            self.policy,
            Policy::OwnUser { .. } | Policy::OwnSettings { .. }
        )
    }

    fn guard(&self, op: Op) -> Option<&'static Guard> {
        self.guards.iter().find(|guard| guard.op == op)
    }

    /// Whether records belong to the user who created them.
    fn user_owned(&self) -> bool {
        matches!(self.policy, Policy::OwnRecords | Policy::RawAttachment)
    }

    fn is_deferred(&self) -> bool {
        matches!(self.policy, Policy::Deferred(_))
    }
}

const TASK_SERVICE: &str = "a later release";

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

// Base models. Settings Administration (base.group_system) is implied by no other group
// (addons/base/data/bootstrap_groups.xml), so a denial of an administrator-only operation names
// it alone; an operation no access grants at all is refused with "No group may do this."
const SETTINGS: &[&str] = &["Settings Administration"];
// Spec cases behind the base expectations: SEC-001 (no grant, no access), SEC-003 (grants OR),
// SEC-005 (a grant covers the operations it names and no other), SEC-008 (model-level message).
const GRANT_BASIS: &[&str] = &["", "", "", ""];
// SEC-068 (direct company restriction), SEC-009/-010 (record message, company hint).
const BASE_COMPANY_BASIS: &[&str] = &["", "", "", "", "", ""];
// SEC-082 (contacts and users by company, internal users' contacts escape), SEC-074 (direct
// company restriction of the rates).
const PARTNER_BASIS: &[&str] = &["", "", "", ""];
const RATE_BASIS: &[&str] = &["", "", "", "", ""];
// COMPANY-006/-009/-012: a user reads its own companies and changes none.
const COMPANY_RECORD_BASIS: &[&str] = &["", "", "", "", ""];
// SEC-033 (self-service fields of the own user), PREFS-004 (own user record only).
const USER_BASIS: &[&str] = &["", "", "", "", ""];
// SEC-022 (a field's groups), (own settings row).
const SETTINGS_ROW_BASIS: &[&str] = &["", "", ""];
// BASE-184: administrators manage System Parameters, other users are refused.
const PARAMETER_BASIS: &[&str] = &["", "", ""];
// TRUST-002 (revisions are selected deterministically, administrators write them); the model
// keeps every revision: delete is refused, archive instead (addons/base ir_approval_policy).
const APPROVAL_BASIS: &[&str] = &["", "", ""];
// TRUST-027: Settings storage changes only through the audited Settings service (the refusals
// of the audit model). CRUD-029: a read skips an identity that names no record.
const SETTINGS_BASIS: &[&str] = &["", "", "", ""];
// JOBS-021 (read-only reflection of the queue), JOBS-023 (create, write, unlink are refused with
// a user error).
const JOB_BASIS: &[&str] = &["", "", "", ""];
// SEC-075 (ir.default: administrators read), SEC-076 (ir.sequence and its date ranges: users
// read), CRUD-029 (a read skips an identity that names no record). Model-level grants only:
// the company restriction of these models needs records, and nobody creates one through RPC in
// this profile, so their record isolation is not exercised here.
const DEFAULT_BASIS: &[&str] = &["", "", "", ""];
const SEQUENCE_BASIS: &[&str] = &["", "", "", ""];
// A model no record of which exists in this profile: the grants, and CRUD-029 for the read.
const NO_ROW_BASIS: &[&str] = &["", "", "", "", ""];
// MOD-047 (an installed module row is never removed), HUMAN-025 (module metadata needs system
// authority).
const MODULE_BASIS: &[&str] = &["", "", "", ""];
// VDESC-032 (a user gets the shared filters and its own, never another user's), SEC-012 (search
// applies the read restriction), SEC-020 (a created record must satisfy the create restriction),
// SEC-009 (record-level message), REL-041 (a linked record must be readable by the caller).
const FILTER_BASIS: &[&str] = &["", "", "", "", ""];
// BASE-099: a standalone attachment is reachable by its creator and by administrators only.
const ATTACHMENT_BASIS: &[&str] = &["", "", ""];
// A print job belongs to the user who asked for it; nobody writes one through RPC. Model-level
// grants only: no print job exists here, so the own-jobs restriction is not exercised.
const PRINT_JOB_BASIS: &[&str] = &["", "", "", ""];

const ADMIN_ALL: Grants = Grants {
    read: Level::Admin,
    create: Level::Admin,
    write: Level::Admin,
    unlink: Level::Admin,
};
const NO_GRANTS: Grants = Grants {
    read: Level::Nobody,
    create: Level::Nobody,
    write: Level::Nobody,
    unlink: Level::Nobody,
};
const ADMIN_READS: Grants = Grants {
    read: Level::Admin,
    create: Level::Nobody,
    write: Level::Nobody,
    unlink: Level::Nobody,
};
const ALL_READ: Grants = Grants {
    read: Level::Internal,
    create: Level::Nobody,
    write: Level::Nobody,
    unlink: Level::Nobody,
};
// The Settings access facade: every internal user reads and saves through the Settings service,
// which checks read and write on this model and then the groups of each setting
// (addons/base/src/ir_config_setting.rs); the model has no field and no row, and nobody creates
// or deletes one. No spec case states these grants; the cells prove the model-level half only.
const SETTINGS_FACADE: Grants = Grants {
    read: Level::Internal,
    create: Level::Nobody,
    write: Level::Internal,
    unlink: Level::Nobody,
};

const fn refusal(
    op: Op,
    status: u16,
    name: &'static str,
    message: &'static str,
    before_access: bool,
) -> Guard {
    Guard {
        op,
        status,
        name,
        message,
        before_access,
    }
}

const JOB_READ_ONLY: &str =
    "ir.job records are read-only; manage background jobs through operator actions";
const JOB_RUN_READ_ONLY: &str =
    "ir.job.run records are read-only; manage background jobs through operator actions";
const JOB_GUARDS: &[Guard] = &[
    refusal(Op::Create, 422, USER_ERROR, JOB_READ_ONLY, true),
    refusal(Op::Write, 422, USER_ERROR, JOB_READ_ONLY, true),
    refusal(Op::Unlink, 422, USER_ERROR, JOB_READ_ONLY, true),
];
const JOB_RUN_GUARDS: &[Guard] = &[
    refusal(Op::Create, 422, USER_ERROR, JOB_RUN_READ_ONLY, true),
    refusal(Op::Write, 422, USER_ERROR, JOB_RUN_READ_ONLY, true),
    refusal(Op::Unlink, 422, USER_ERROR, JOB_RUN_READ_ONLY, true),
];
const SETTINGS_STORAGE: &str =
    "Settings storage can only be changed by the audited Settings service.";
const AUDIT_GUARDS: &[Guard] = &[
    refusal(Op::Create, 403, ACCESS_ERROR, SETTINGS_STORAGE, true),
    refusal(Op::Write, 403, ACCESS_ERROR, SETTINGS_STORAGE, true),
    refusal(Op::Unlink, 403, ACCESS_ERROR, SETTINGS_STORAGE, true),
];
// The key table of the typed actions refuses every direct change as a programming error, which
// the transport reports as a 500 (addons/base/src/ir_actions/mod.rs, `key_table_refusal`; the
// same refusal is asserted in crates/erp-test-models/tests/ui_action_specs.rs). The cells record
// that behaviour. A 4xx kind would fit a caller's mistake better: any internal user can raise
// this 500; reported to the lead with this slice.
const ACTION_KEY: &str =
    "ir.actions.actions rows follow their typed action; write the typed action instead";
const ACTION_KEY_GUARDS: &[Guard] = &[
    refusal(Op::Create, 500, PROGRAMMING_ERROR, ACTION_KEY, true),
    refusal(Op::Write, 500, PROGRAMMING_ERROR, ACTION_KEY, true),
    refusal(Op::Unlink, 500, PROGRAMMING_ERROR, ACTION_KEY, true),
];
const APPROVAL_GUARDS: &[Guard] = &[refusal(
    Op::Unlink,
    422,
    VALIDATION_ERROR,
    "Approval policy revisions cannot be deleted; archive them instead.",
    false,
)];

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
        guards: &[],
    }
}

const fn mail(model: &'static str) -> ModelCase {
    specialized(model, "mail", false)
}

const fn service(model: &'static str, addon: &'static str) -> ModelCase {
    specialized(model, addon, false)
}

const fn transient(model: &'static str) -> ModelCase {
    specialized(model, "base", true)
}

const fn specialized(model: &'static str, addon: &'static str, transient: bool) -> ModelCase {
    ModelCase {
        model,
        addon,
        description: "",
        transient,
        basis: &[],
        scope: Scope::Global,
        policy: Policy::Specialized,
        guards: &[],
    }
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
        guards: &[],
    }
}

/// A base model whose access is a table of grants.
const fn base(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
    grants: Grants,
    rows: Rows,
    guards: &'static [Guard],
) -> ModelCase {
    ModelCase {
        model,
        addon: "base",
        description,
        transient: false,
        basis,
        scope: Scope::Global,
        policy: Policy::Base { grants, rows },
        guards,
    }
}

/// A base model with user-owned records.
const fn owned(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
    policy: Policy,
) -> ModelCase {
    ModelCase {
        model,
        addon: "base",
        description,
        transient: false,
        basis,
        scope: Scope::Owner,
        policy,
        guards: &[],
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
        guards: &[],
    }
}

/// A base model every internal user reads and only Settings manages.
const fn reference(model: &'static str, description: &'static str) -> ModelCase {
    read_only(model, "base", description, GRANT_BASIS, SETTINGS)
}

/// A base model only Settings may see or change.
const fn manager_only(model: &'static str, description: &'static str) -> ModelCase {
    base(
        model,
        description,
        GRANT_BASIS,
        ADMIN_ALL,
        Rows::Created,
        &[],
    )
}

/// A base model whose seeded records are the fixture's own (companies, users, settings rows).
const fn fixture_owned(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
    policy: Policy,
) -> ModelCase {
    ModelCase {
        model,
        addon: "base",
        description,
        transient: false,
        basis,
        scope: Scope::Fixture,
        policy,
        guards: &[],
    }
}

/// A base model with a direct company restriction that only Settings administrators change.
const fn base_company(
    model: &'static str,
    description: &'static str,
    basis: &'static [&'static str],
) -> ModelCase {
    ModelCase {
        addon: "base",
        policy: Policy::CompanyScoped {
            groups: SETTINGS,
            company_hint: true,
        },
        ..company(model, description, basis)
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
    ("image.mixin", "abstract picture mixin with resized copies"),
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
    fixture_owned(
        "res.company",
        "Company",
        COMPANY_RECORD_BASIS,
        Policy::OwnCompany { groups: SETTINGS },
    ),
    reference("res.country", "Country"),
    reference("res.country.group", "Country Group"),
    reference("res.country.state", "Country state"),
    reference("res.currency", "Currency"),
    base_company("res.currency.rate", "Currency Rate", RATE_BASIS),
    reference("res.currency.rate.type", "Currency Rate Type"),
    manager_only("res.groups", "Access Groups"),
    manager_only("res.groups.privilege", "Privilege"),
    reference("res.lang", "Language"),
    base_company("res.partner", "Contact", PARTNER_BASIS),
    reference("res.partner.category", "Partner Tag"),
    reference("res.partner.industry", "Industry"),
    fixture_owned(
        "res.users",
        "User",
        USER_BASIS,
        Policy::OwnUser { groups: SETTINGS },
    ),
    fixture_owned(
        "res.users.settings",
        "User Settings",
        SETTINGS_ROW_BASIS,
        Policy::OwnSettings { groups: SETTINGS },
    ),
    // ir.* with access rows
    // Precisions ship with product and uom; administrators read them, nobody writes one by RPC.
    base(
        "decimal.precision",
        "Decimal Precision",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.access",
        "Access",
        GRANT_BASIS,
        ADMIN_ALL,
        Rows::Created,
        &[],
    ),
    base(
        "ir.actions.act_url",
        "Action URL",
        NO_ROW_BASIS,
        ADMIN_READS,
        Rows::Absent,
        &[],
    ),
    read_only(
        "ir.actions.act_window",
        "base",
        "Action Window",
        GRANT_BASIS,
        SETTINGS,
    ),
    read_only(
        "ir.actions.act_window.view",
        "base",
        "Action Window View",
        GRANT_BASIS,
        SETTINGS,
    ),
    base(
        "ir.actions.actions",
        "Action",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        ACTION_KEY_GUARDS,
    ),
    base(
        "ir.actions.client",
        "Client Action",
        // internal users read client actions so a menu can open one in the caller's rights.
        GRANT_BASIS,
        ALL_READ,
        Rows::Absent,
        &[],
    ),
    read_only(
        "ir.actions.report",
        "base",
        "Report Action",
        GRANT_BASIS,
        SETTINGS,
    ),
    base(
        "ir.actions.server",
        "Server Actions",
        NO_ROW_BASIS,
        ADMIN_READS,
        Rows::Absent,
        &[],
    ),
    base(
        "ir.approval.policy",
        "Approval Policy Revision",
        APPROVAL_BASIS,
        ADMIN_ALL,
        Rows::Created,
        APPROVAL_GUARDS,
    ),
    base(
        "ir.approval.request",
        "Approval Request",
        APPROVAL_BASIS,
        NO_GRANTS,
        Rows::Absent,
        &[],
    ),
    owned(
        "ir.attachment",
        "Attachment",
        ATTACHMENT_BASIS,
        Policy::RawAttachment,
    ),
    base(
        "ir.config.setting",
        "Settings",
        NO_ROW_BASIS,
        SETTINGS_FACADE,
        Rows::Absent,
        &[],
    ),
    base(
        "ir.config.setting.audit",
        "Settings Audit",
        SETTINGS_BASIS,
        ADMIN_READS,
        Rows::Absent,
        AUDIT_GUARDS,
    ),
    base_company("ir.dated.parameter", "Dated Parameter", BASE_COMPANY_BASIS),
    base(
        "ir.default",
        "Default Values",
        DEFAULT_BASIS,
        ADMIN_READS,
        Rows::Absent,
        &[],
    ),
    base(
        "ir.field.access",
        "Field Access",
        GRANT_BASIS,
        ADMIN_ALL,
        Rows::Created,
        &[],
    ),
    owned("ir.filters", "Filter", FILTER_BASIS, Policy::OwnRecords),
    base(
        "ir.job",
        "Queue Job",
        JOB_BASIS,
        ADMIN_READS,
        Rows::Volatile,
        JOB_GUARDS,
    ),
    base(
        "ir.job.run",
        "Job Execution History",
        JOB_BASIS,
        ADMIN_READS,
        Rows::Volatile,
        JOB_RUN_GUARDS,
    ),
    base(
        "ir.model",
        "Models",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.model.data",
        "Model Data",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.model.fields",
        "Fields",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.model.fields.selection",
        "Fields Selection",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.model.relation",
        "Model Relations",
        GRANT_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    base(
        "ir.config_parameter",
        "System Parameter",
        PARAMETER_BASIS,
        ADMIN_ALL,
        Rows::Created,
        &[],
    ),
    base(
        "ir.module.module",
        "Module",
        MODULE_BASIS,
        ADMIN_READS,
        Rows::Shipped,
        &[],
    ),
    // No sequence ships with this profile and nobody creates one by RPC.
    base(
        "ir.sequence",
        "Sequence",
        SEQUENCE_BASIS,
        ALL_READ,
        Rows::Absent,
        &[],
    ),
    base(
        "ir.sequence.date_range",
        "Sequence Date Range",
        SEQUENCE_BASIS,
        ALL_READ,
        Rows::Absent,
        &[],
    ),
    read_only("ir.ui.menu", "base", "Menu", GRANT_BASIS, SETTINGS),
    base(
        "ir.ui.view",
        "View",
        GRANT_BASIS,
        ADMIN_ALL,
        Rows::Created,
        &[],
    ),
    // A print job is created by the print service for the user who asked; a user reads its own.
    base(
        "report.job",
        "Print Job",
        PRINT_JOB_BASIS,
        ALL_READ,
        Rows::Absent,
        &[],
    ),
    read_only(
        "report.paperformat",
        "base",
        "Paper Format Config",
        GRANT_BASIS,
        SETTINGS,
    ),
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

/// A cell whose contract outcome the product does not give yet. The cell asserts the present
/// reply exactly and that the record is untouched, so the fix turns it red and removes the entry.
struct Defect {
    model: &'static str,
    who: Who,
    op: Op,
    /// The board task that fixes the product.
    task: &'static str,
    status: u16,
    name: &'static str,
    message: &'static str,
}

/// Finding MU-R03: with the uom addon installed, deleting any user ends in a database error of
/// the unit reference guard instead of removing the user.
const KNOWN_DEFECTS: &[Defect] = &[Defect {
    model: "res.users",
    who: Who::Admin,
    op: Op::Unlink,
    task: "a later release",
    status: 500,
    name: "erp.database",
    message: "A database error occurred (SQLSTATE UOM05).",
}];

fn known_defect(case: &ModelCase, who: Who, op: Op) -> Option<&'static Defect> {
    KNOWN_DEFECTS
        .iter()
        .find(|defect| defect.model == case.model && defect.who == who && defect.op == op)
}

/// The expected outcome of one cell, from the contract.
pub fn expect(case: &ModelCase, who: Who, op: Op) -> Expect {
    if who == Who::Anon {
        return Expect::Unauthenticated;
    }
    match case.policy {
        Policy::Deferred(task) => Expect::Deferred(task),
        Policy::Specialized => unreachable!("the specialized matrix owns the authenticated cases"),
        Policy::InternalReadOnly { .. } => match (who, op) {
            (_, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) | (_, Op::Read) => Expect::Ok,
            _ => Expect::ModelDenied,
        },
        Policy::CompanyScoped { .. } | Policy::OwnCompany { .. } => match (who, op) {
            (Who::Admin, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) => Expect::Ok,
            (_, Op::Search) => Expect::Visible(Visible::OwnAndShared),
            (_, Op::Read) => Expect::ReadByCompany,
            _ => Expect::ModelDenied,
        },
        // the write cell is the own record and a self-service field; `own_user_extras` covers the
        // own login and another user's record
        Policy::OwnUser { .. } => match (who, op) {
            (Who::Admin, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) | (_, Op::Write) => Expect::Ok,
            (_, Op::Search) => Expect::Visible(Visible::OwnAndShared),
            (_, Op::Read) => Expect::ReadByCompany,
            (_, Op::Create | Op::Unlink) => Expect::ModelDenied,
        },
        // write and unlink cells are the own row; `own_settings_extras` covers another user's row
        Policy::OwnSettings { .. } => match (who, op) {
            (Who::Admin, Op::Search) => Expect::Visible(Visible::All),
            (Who::Admin, _) | (_, Op::Write | Op::Unlink) => Expect::Ok,
            (_, Op::Search) => Expect::Visible(Visible::OwnAndShared),
            (_, Op::Read) => Expect::ReadByCompany,
            (_, Op::Create) => Expect::FieldDenied("user_id"),
        },
        Policy::Base { grants, rows } => {
            let guard = case.guard(op);
            if let Some(guard) = guard
                && guard.before_access
            {
                return Expect::Refused(guard);
            }
            let granted = match grants.of(op) {
                Level::Nobody => false,
                Level::Admin => who == Who::Admin,
                Level::Internal => true,
            };
            match (granted, guard, op, rows) {
                (false, ..) => Expect::ModelDenied,
                (true, Some(guard), ..) => Expect::Refused(guard),
                (true, None, Op::Search, _) => Expect::Visible(Visible::All),
                // no record exists: the identity the cell uses names none
                (true, None, Op::Read, Rows::Absent | Rows::Volatile) => Expect::NoRecord,
                (true, None, Op::Write, Rows::Absent | Rows::Volatile) => Expect::Missing,
                (true, None, ..) => Expect::Ok,
            }
        }
        // the administrator has no grant of its own here: it is an internal user like A and B
        Policy::OwnRecords => match op {
            Op::Search => Expect::Visible(Visible::OwnAndShared),
            Op::Read => Expect::ReadByCompany,
            Op::Create | Op::Write | Op::Unlink => Expect::Ok,
        },
        Policy::RawAttachment => match (who, op) {
            (Who::Admin, Op::Search) => Expect::Visible(Visible::All),
            (_, Op::Search) => Expect::Visible(Visible::OwnAndShared),
            (Who::Admin, _) => Expect::Ok,
            (_, Op::Read) => Expect::ReadByCompany,
            (_, Op::Create | Op::Write | Op::Unlink) => Expect::Ok,
        },
    }
}

/// The expected outcome when `who` changes or deletes a record another subject owns; `None` for
/// models whose records have no owner.
pub fn expect_foreign(case: &ModelCase, who: Who, op: Op) -> Option<Expect> {
    match (case.policy, who, op) {
        // A record created for another user: the administrator reads every user, so the record is
        // inserted and then fails its own create restriction; A and B read only
        // their own user record, so the link to the other user is refused first.
        (Policy::OwnRecords, Who::Admin, Op::Create) => Some(Expect::RecordDenied),
        (Policy::OwnRecords, _, Op::Create) => Some(Expect::UnreadableUser),
        (Policy::OwnRecords, _, Op::Write | Op::Unlink) => Some(Expect::RecordDenied),
        (Policy::RawAttachment, Who::Admin, Op::Write | Op::Unlink) => Some(Expect::Ok),
        (Policy::RawAttachment, _, Op::Write | Op::Unlink) => Some(Expect::DocumentDenied),
        _ => None,
    }
}

/// Structural problems of the manifest itself (no server needed).
pub fn manifest_problems() -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    let mut specialized: BTreeSet<_> = super::multiuser_rpc_special::MAIL_MODELS
        .iter()
        .chain(super::multiuser_rpc_special::SERVICE_MODELS)
        .chain(super::multiuser_rpc_special::TRANSIENT_MODELS)
        .copied()
        .collect();
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
                if task != TASK_SERVICE {
                    problems.push(format!("{} defers to unknown task {task}", case.model));
                }
            }
            Policy::Specialized => {
                if !specialized.remove(case.model) {
                    problems.push(format!("{} lacks a specialized matrix case", case.model));
                }
            }
            Policy::InternalReadOnly { groups }
            | Policy::CompanyScoped { groups, .. }
            | Policy::OwnCompany { groups }
            | Policy::OwnUser { groups }
            | Policy::OwnSettings { groups } => {
                if case.basis.is_empty() || case.description.is_empty() || groups.is_empty() {
                    problems.push(format!(
                        "{} lacks basis, description or granting groups",
                        case.model
                    ));
                }
                let wanted = match case.policy {
                    Policy::CompanyScoped { .. } => Scope::Company,
                    Policy::OwnCompany { .. }
                    | Policy::OwnUser { .. }
                    | Policy::OwnSettings { .. } => Scope::Fixture,
                    _ => Scope::Global,
                };
                if case.scope != wanted {
                    problems.push(format!("{}: scope and policy disagree", case.model));
                }
            }
            Policy::Base { grants, rows } => {
                if case.basis.is_empty() || case.description.is_empty() {
                    problems.push(format!("{} lacks basis or description", case.model));
                }
                if case.scope != Scope::Global {
                    problems.push(format!("{}: scope and policy disagree", case.model));
                }
                // records are created through RPC exactly when somebody holds create
                if (rows == Rows::Created) == (grants.create == Level::Nobody) {
                    problems.push(format!(
                        "{}: the row source and the create grant disagree",
                        case.model
                    ));
                }
            }
            Policy::OwnRecords | Policy::RawAttachment => {
                if case.basis.is_empty() || case.description.is_empty() {
                    problems.push(format!("{} lacks basis or description", case.model));
                }
                if case.scope != Scope::Owner {
                    problems.push(format!("{}: scope and policy disagree", case.model));
                }
            }
        }
        let mut guarded = BTreeSet::new();
        for guard in case.guards {
            if matches!(guard.op, Op::Search | Op::Read) || !guarded.insert(guard.op.method()) {
                problems.push(format!(
                    "{}: a guard is on a read operation or listed twice",
                    case.model
                ));
            }
            if guard.message.is_empty() || guard.name.is_empty() {
                problems.push(format!("{}: a guard lacks its error", case.model));
            }
        }
    }
    for model in specialized {
        problems.push(format!("{model} is tested outside the manifest"));
    }
    for defect in KNOWN_DEFECTS {
        let written = MANIFEST
            .iter()
            .any(|case| case.model == defect.model && !case.is_deferred());
        if !written || defect.who == Who::Anon || defect.task.is_empty() {
            problems.push(format!(
                "known defect of {} names no written cell or no task",
                defect.model
            ));
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
const USER_ERROR: &str = "erp.exceptions.UserError";
const VALIDATION_ERROR: &str = "erp.exceptions.ValidationError";
const MISSING_ERROR: &str = "erp.exceptions.MissingError";
const PROGRAMMING_ERROR: &str = "erp.programming";
const UNAUTHORIZED: &str = "werkzeug.exceptions.Unauthorized";
const UNAUTHENTICATED_MESSAGE: &str =
    "User not authenticated, use an API Key with a Bearer Authorization header.";
/// The SEC-008 model-level text (`crates/erp-security/src/explain.rs`, `model_message`), built from
/// the contract's granting groups.
fn model_message(case: &ModelCase, op: Op) -> String {
    let groups: Vec<String> = case.groups(op).iter().map(|g| format!("\t- {g}")).collect();
    let group_info = if groups.is_empty() {
        "No group may do this.".to_owned()
    } else {
        format!("Groups that may do this:\n{}", groups.join("\n"))
    };
    format!(
        "Your access rights do not let you {} '{}' ({}) records.\n\n{group_info}\n\nAsk an administrator if you need this access.",
        op.verb(),
        case.description,
        case.model
    )
}

/// The start of the field-level text (`crates/erp-orm/src/field_access.rs`, `field_denied`) for a
/// refused write of `field`. The text may continue with the subject's id and groups, which the
/// contract does not fix, so the judge compares this beginning only.
fn field_message(case: &ModelCase, field: &str) -> String {
    format!(
        "You do not have enough rights to access the field \"{field}\" on {} ({}). Please contact your system administrator.\n\nOperation: write",
        case.description, case.model
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
    record_text(case.description, case.model, op, name, id, hint)
}

/// The same text for any model, by its description and name.
fn record_text(
    description: &str,
    model: &str,
    op: Op,
    name: &str,
    id: i64,
    hint: &Hint<'_>,
) -> String {
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
        description,
        model
    )
}

/// The refusal of a saved filter that names a user the caller cannot read: the field, then the
/// record-level read denial of the user model (`crates/erp-orm/src/commands.rs`).
/// "User" is the description of `res.users` as the server names it in that denial.
fn unreadable_user_message(name: &str, id: i64) -> String {
    format!(
        "Failed to write field ir.filters.user_ids\n{}",
        record_text("User", "res.users", Op::Read, name, id, &Hint::None)
    )
}

/// The attachment refusal of a standalone attachment the caller did not create
/// (`addons/base/src/attachments/access.rs`, `denied`).
fn document_message(op: Op, ids: &[i64], uid: i64) -> String {
    format!(
        "Sorry, you are not allowed to access this document. Please contact your system administrator.\n\n(Operation: {})\n\nRecords: {ids:?}, User: {uid}",
        op.method()
    )
}

/// The text for an identity that names no record (`crates/erp-orm/src/read.rs`, `missing_error`).
fn missing_message(case: &ModelCase, id: i64, uid: i64) -> String {
    format!(
        "Record does not exist or has been deleted.\n(Record: {}({id},), User: {uid})",
        case.model
    )
}

/// Compare one outcome with the expected one; `record` is the subject and hint a record-level
/// denial must name, `ids` the identities the call named.
fn judge(
    case: &ModelCase,
    op: Op,
    expect: Expect,
    out: &Outcome,
    record: Option<(&str, i64, Hint<'_>)>,
    ids: &[i64],
) -> Option<String> {
    let denied = out.status == 403 && out.name == ACCESS_ERROR;
    let fine = match expect {
        Expect::Ok | Expect::Visible(_) | Expect::ReadByCompany => out.status == 200,
        Expect::NoRecord => {
            out.status == 200
                && out
                    .value
                    .as_ref()
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
        }
        Expect::ModelDenied => denied && out.message == model_message(case, op),
        Expect::RecordDenied => match &record {
            Some((name, id, hint)) => {
                let hint = match case.policy {
                    // an ownership rule names no company, so no company hint follows; the
                    // refusing grant of the fixture-owned models names the principal or its
                    // companies, and the record itself carries no company
                    Policy::CompanyScoped {
                        company_hint: false,
                        ..
                    }
                    | Policy::OwnRecords
                    | Policy::RawAttachment
                    | Policy::OwnCompany { .. }
                    | Policy::OwnUser { .. }
                    | Policy::OwnSettings { .. } => &Hint::None,
                    Policy::CompanyScoped { .. }
                    | Policy::InternalReadOnly { .. }
                    | Policy::Base { .. }
                    | Policy::Deferred(_)
                    | Policy::Specialized => hint,
                };
                denied && out.message == record_message(case, op, name, *id, hint)
            }
            None => false,
        },
        Expect::FieldDenied(field) => {
            denied && out.message.starts_with(&field_message(case, field))
        }
        Expect::Unauthenticated => {
            out.status == 401 && out.name == UNAUTHORIZED && out.message == UNAUTHENTICATED_MESSAGE
        }
        Expect::Refused(guard) => {
            out.status == guard.status && out.name == guard.name && out.message == guard.message
        }
        Expect::Missing => match (&record, ids) {
            (Some((_, uid, _)), [id]) => {
                out.status == 404
                    && out.name == MISSING_ERROR
                    && out.message == missing_message(case, *id, *uid)
            }
            _ => false,
        },
        Expect::DocumentDenied => match &record {
            Some((_, uid, _)) => denied && out.message == document_message(op, ids, *uid),
            None => false,
        },
        Expect::UnreadableUser => match &record {
            Some((name, uid, _)) => denied && out.message == unreadable_user_message(name, *uid),
            None => false,
        },
        // a deferred cell is skipped before it is judged; reaching here is a runner defect
        Expect::Deferred(task) => {
            return Some(format!("a cell deferred to {task} reached the judge"));
        }
    };
    (!fine).then(
        || match (expect, out.value.as_ref().and_then(Value::as_array)) {
            // the number of rows only, never their content
            (Expect::NoRecord, Some(rows)) if out.status == 200 => {
                format!(
                    "expected {expect:?}, got HTTP 200 with {} row(s)",
                    rows.len()
                )
            }
            _ => format!("expected {expect:?}, got {}", out.brief()),
        },
    )
}

/// The one simple field a write changes, and how its value is compared.
enum Marker {
    Char(&'static str),
    Int(&'static str),
    /// An integer with a small upper bound.
    Small(&'static str),
    Bool(&'static str),
    Float(&'static str),
    /// A field that accepts only certain texts, with the value to write.
    Fixed(&'static str, &'static str),
}

impl Marker {
    /// The marker of `model`; `None` for a model without any declared field.
    fn of(model: &str) -> Option<Self> {
        Some(match model {
            "ir.config.setting" => return None,
            "decimal.precision" => Marker::Small("digits"),
            "ir.access" | "ir.actions.actions" | "ir.filters" | "ir.ui.view" => {
                Marker::Char("name")
            }
            "ir.actions.act_url" => Marker::Char("url"),
            "ir.actions.act_window" => Marker::Int("limit"),
            "ir.actions.client" => Marker::Char("res_model"),
            "ir.actions.report" => Marker::Char("report_file"),
            "ir.approval.policy" => Marker::Bool("active"),
            "ir.approval.request" => Marker::Fixed("state", "pending"),
            "ir.attachment" => Marker::Char("description"),
            "ir.config.setting.audit" => Marker::Char("setting_key"),
            "ir.config_parameter" | "ir.dated.parameter" => Marker::Char("value"),
            "ir.default" => Marker::Char("condition"),
            "ir.field.access" => Marker::Char("note"),
            "ir.job" | "ir.job.run" => Marker::Char("kind"),
            "ir.model" => Marker::Char("info"),
            "ir.model.data" | "ir.model.relation" => Marker::Char("module"),
            "ir.model.fields" => Marker::Char("field_description"),
            "ir.sequence" => Marker::Char("prefix"),
            "ir.sequence.date_range" => Marker::Int("number_next"),
            "report.job" => Marker::Int("total"),
            "report.paperformat" => Marker::Int("header_spacing"),
            "ir.actions.act_window.view"
            | "ir.actions.server"
            | "ir.model.fields.selection"
            | "ir.module.module"
            | "ir.ui.menu" => Marker::Int("sequence"),
            "product.category"
            | "uom.uom"
            | "res.company"
            | "res.country.group"
            | "res.country.state"
            | "res.currency.rate.type"
            | "res.groups"
            | "res.groups.privilege"
            | "res.lang"
            | "res.partner.category"
            | "res.partner.industry" => Marker::Char("name"),
            "res.country" => Marker::Int("phone_code"),
            "res.currency" => Marker::Char("full_name"),
            "res.currency.rate" => Marker::Float("rate"),
            "res.partner" => Marker::Char("ref"),
            // a self-service field of the own user that is not part of any message
            "res.users" => Marker::Fixed("tz", "Asia/Jakarta"),
            "res.users.settings" => Marker::Fixed("homemenu_config", "[1]"),
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
        })
    }

    fn field(&self) -> &'static str {
        match self {
            Marker::Char(f)
            | Marker::Int(f)
            | Marker::Small(f)
            | Marker::Bool(f)
            | Marker::Float(f)
            | Marker::Fixed(f, _) => f,
        }
    }

    fn new_value(&self, tag: &str) -> Value {
        match self {
            Marker::Char(_) => json!(format!("{tag} written")),
            Marker::Int(_) => json!(77),
            Marker::Small(_) => json!(7),
            Marker::Bool(_) => json!(false),
            Marker::Float(_) => json!(12.5),
            Marker::Fixed(_, value) => json!(value),
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
#[derive(Default)]
struct Seeds {
    /// The administrator's own record (user-owned models only).
    admin: Option<i64>,
    a: Option<i64>,
    b: Option<i64>,
    shared: Option<i64>,
}

impl Seeds {
    fn all(&self) -> Vec<i64> {
        [self.admin, self.a, self.b, self.shared]
            .into_iter()
            .flatten()
            .collect()
    }

    fn own(&self, who: Who) -> Option<i64> {
        match who {
            Who::Admin => self.admin,
            Who::A => self.a,
            Who::B => self.b,
            Who::Anon => None,
        }
    }

    /// Seeded records that are neither `who`'s own nor shared.
    fn others(&self, who: Who) -> Vec<i64> {
        let own = self.own(who);
        [self.admin, self.a, self.b]
            .into_iter()
            .flatten()
            .filter(|id| Some(*id) != own)
            .collect()
    }

    fn visible(&self, who: Who, visible: Visible) -> BTreeSet<i64> {
        match visible {
            Visible::All => self.all().into_iter().collect(),
            Visible::OwnAndShared => self.own(who).into_iter().chain(self.shared).collect(),
        }
    }
}

/// Whose a record is: the company it belongs to, and for user-owned models the subject who
/// creates it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Co {
    A,
    B,
    /// The administrator's: no company; created by the administrator on user-owned models.
    Admin,
    Shared,
}

impl Co {
    fn owner(self) -> Option<Who> {
        match self {
            Co::A => Some(Who::A),
            Co::B => Some(Who::B),
            Co::Admin => Some(Who::Admin),
            Co::Shared => None,
        }
    }
}

/// Models whose records belong to the user who created them: such a record is created by its
/// owner, never by the provisioning session.
fn user_owned(model: &str) -> bool {
    MANIFEST
        .iter()
        .any(|case| case.model == model && case.user_owned())
}

struct Base {
    uom: i64,
    currency: i64,
    partner: i64,
    rate_type: i64,
}

/// Runner over one started multi-user fixture.
pub struct Crud<'a, 'f> {
    fx: &'a MultiuserFixture<'f>,
    base: OnceCell<Base>,
    /// Approval revisions made so far: each takes the next manifest model as its selector.
    revisions: Cell<usize>,
    serial: Cell<u32>,
}

/// The text of a caught panic (a `panic!` with a message carries a `String` or a `&str`).
fn panic_text(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("a panic without a text message")
}

/// A tag reduced to lower-case letters, digits and underscores, for keys with a strict format.
fn slug(tag: &str) -> String {
    tag.chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => c,
            _ => '_',
        })
        .collect()
}

fn ids_of(value: &Value) -> Option<Vec<i64>> {
    value.as_array()?.iter().map(Value::as_i64).collect()
}

impl<'a, 'f> Crud<'a, 'f> {
    pub fn new(fx: &'a MultiuserFixture<'f>) -> Self {
        Self {
            fx,
            base: OnceCell::new(),
            revisions: Cell::new(0),
            serial: Cell::new(0),
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
            Co::Admin | Co::Shared => json!(false),
        }
    }

    fn co_of(who: Who) -> Co {
        match who {
            Who::A => Co::A,
            Who::B => Co::B,
            Who::Admin => Co::Admin,
            Who::Anon => Co::Shared,
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

    /// A setup or readback call: by the provisioning session, or by the subject who keeps the
    /// record (a user-owned record is created by its owner, and only its owner reads it back when
    /// nobody else may).
    fn keep(
        &self,
        keeper: Option<Who>,
        model: &str,
        method: &str,
        mut body: Value,
        extra: &[(&str, Value)],
    ) -> Result<Value, String> {
        let Some(who) = keeper else {
            return self.prov_with(model, method, body, extra);
        };
        let mut context = json!({"allowed_company_ids": self.allowed(who)});
        for (key, value) in extra {
            context[*key] = value.clone();
        }
        body["context"] = context;
        let out = classify(&self.fx.rpc(who.subject(), model, method, body));
        match out.value {
            Some(value) if out.status == 200 => Ok(value),
            _ => Err(format!(
                "{model}/{method} as {}: {}",
                who.label(),
                out.brief()
            )),
        }
    }

    /// Who creates a record of `model` that belongs to `co`.
    fn creator(model: &str, co: Co) -> Option<Who> {
        if user_owned(model) { co.owner() } else { None }
    }

    /// Who reads a record of `model` back. Only the owner reads its own saved filter; everything
    /// else is read by the provisioning administrator.
    fn reader(model: &str, co: Co) -> Option<Who> {
        if model == "ir.filters" {
            co.owner()
        } else {
            None
        }
    }

    fn call(&self, who: Who, model: &str, method: &str, body: Value) -> Outcome {
        self.call_in(who, &self.allowed(who), model, method, body)
    }

    /// A search as a test subject that also returns archived records: whether a record is
    /// archived is not an access matter, and some seeds start archived (a new language).
    fn search_all(&self, who: Who, model: &str, domain: Value) -> Outcome {
        let mut body = json!({"domain": domain});
        if who != Who::Anon {
            body["context"] = json!({
                "allowed_company_ids": self.allowed(who),
                "active_test": false,
            });
        }
        classify(&self.fx.rpc(who.subject(), model, "search", body))
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
                rate_type: self.one_id("res.currency.rate.type", json!([["code", "=", "spot"]])),
            }
        })
    }

    /// The next number of this run, for values a unique key covers (codes, logins, rate days).
    fn next(&self) -> u32 {
        let n = self.serial.get() + 1;
        self.serial.set(n);
        n
    }

    /// A two-character code no shipped country or currency uses (those are letters only).
    fn code(&self) -> String {
        let n = self.next();
        let letter = char::from(b'A' + u8::try_from(n % 26).expect("below 26"));
        format!("{}{letter}", (n / 26) % 10)
    }

    fn create_id(&self, model: &str, vals: Value, extra: &[(&str, Value)]) -> i64 {
        self.create_id_as(None, model, vals, extra)
    }

    fn create_id_as(
        &self,
        keeper: Option<Who>,
        model: &str,
        vals: Value,
        extra: &[(&str, Value)],
    ) -> i64 {
        let created = self
            .keep(keeper, model, "create", json!({"vals_list": [vals]}), extra)
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

    /// The `ir.model` row of a model.
    fn model_id(&self, model: &str) -> i64 {
        self.one_id("ir.model", json!([["model", "=", model]]))
    }

    /// The `ir.model.fields` row of a field.
    fn field_id(&self, model: &str, field: &str) -> i64 {
        self.one_id(
            "ir.model.fields",
            json!([["model", "=", model], ["name", "=", field]]),
        )
    }

    /// A model no approval revision of this run names yet (active revisions of one selector are
    /// refused, and the model is part of the selector).
    fn next_revision_model(&self) -> &'static str {
        let n = self.revisions.get();
        self.revisions.set(n + 1);
        MANIFEST
            .get(n)
            .unwrap_or_else(|| panic!("more approval revisions than manifest models"))
            .model
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
            // Base models. Where nobody may create a record the payload is still a plausible one:
            // the refusal must come from the access check, not from a malformed value.
            "decimal.precision" | "ir.sequence" | "ir.ui.menu" | "report.paperformat" => {
                json!({"name": tag})
            }
            "ir.attachment" => json!({"name": tag}),
            "ir.access" => json!({
                "name": tag,
                "model_id": self.model_id("res.partner"),
                "group_id": self.fx.group("group_system"),
                "operation": "r",
            }),
            "ir.actions.act_url" => json!({"name": tag, "url": "/crud-matrix"}),
            "ir.actions.act_window" => json!({"name": tag, "res_model": "res.partner"}),
            "ir.actions.act_window.view" => json!({
                "act_window_id": self.make("ir.actions.act_window", &parent, Co::Shared),
                "view_mode": "list",
            }),
            "ir.actions.actions" => json!({
                "name": tag, "type": "ir.actions.act_window", "binding_type": "action"
            }),
            "ir.actions.client" => json!({"name": tag, "tag": "crud_matrix"}),
            "ir.actions.report" => {
                // a report names an active QWeb template; none ships with this profile
                let key = format!("crud_matrix.{}", slug(tag));
                self.create_id(
                    "ir.ui.view",
                    json!({
                        "name": key,
                        "type": "qweb",
                        "key": key,
                        "arch": format!("<t t-name=\"{key}\"><p>crud matrix</p></t>"),
                    }),
                    &[],
                );
                json!({"name": tag, "model": "res.partner", "report_name": key})
            }
            "ir.actions.server" => json!({
                "name": tag, "state": "code", "model_id": self.model_id("res.partner")
            }),
            // "allow" for a user's read is what holds without any revision, so the revision
            // changes nothing for the other cells
            "ir.approval.policy" => json!({
                "name": tag,
                "principal_kind": "user",
                "effect": "read",
                "outcome": "allow",
                "model": self.next_revision_model(),
            }),
            "ir.approval.request" => json!({
                "target_model": "res.partner", "target_method": "write", "state": "pending"
            }),
            "ir.config.setting" => json!({}),
            "ir.config.setting.audit" => json!({"setting_key": tag}),
            "ir.config_parameter" => {
                json!({"key": format!("crudmatrix.{}", slug(tag)), "value": "v"})
            }
            "ir.dated.parameter" => json!({
                "key": format!("crud.matrix.{}", slug(tag)),
                "date_from": "2026-01-01",
                "value_kind": "text",
                "value": "v",
                "company_id": company,
            }),
            "ir.default" => json!({
                "field_id": self.field_id("res.partner", "ref"), "json_value": "\"crud\""
            }),
            "ir.field.access" => {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static NEXT_FIELD: AtomicUsize = AtomicUsize::new(0);
                const CANDIDATES: &[&str] = &[
                    "ref",
                    "street",
                    "street2",
                    "city",
                    "zip",
                    "vat",
                    "website",
                    "phone",
                    "color",
                    "tz_offset",
                    "country_code",
                    "email_formatted",
                ];
                let field_name =
                    CANDIDATES[NEXT_FIELD.fetch_add(1, Ordering::Relaxed) % CANDIDATES.len()];
                json!({
                    "field_id": self.field_id("res.partner", field_name),
                    "mode": "extend",
                    "group_ids": [[6, 0, [self.fx.group("group_system")]]],
                    "note": tag,
                })
            }
            "ir.filters" => {
                let users: Vec<i64> = co
                    .owner()
                    .map(|who| self.fx.user(who.subject()).id)
                    .into_iter()
                    .collect();
                json!({"name": tag, "model_id": "res.partner", "user_ids": [[6, 0, users]]})
            }
            "ir.job" | "ir.job.run" => json!({"kind": "crud.matrix"}),
            "ir.model" => json!({"name": tag, "model": "x_crud.matrix"}),
            "ir.model.data" => json!({
                "name": slug(tag),
                "model": "res.partner",
                "module": "crudmatrix",
                "res_id": base.partner,
            }),
            "ir.model.fields" => json!({
                "name": "x_crud_matrix",
                "model_id": self.model_id("res.partner"),
                "model": "res.partner",
                "ttype": "char",
            }),
            "ir.model.fields.selection" => json!({
                "field_id": self.field_id("ir.access", "operation"),
                "value": "crud_matrix",
                "name": tag,
            }),
            "ir.model.relation" => json!({
                "name": "crud_matrix_rel", "model_id": self.model_id("res.partner")
            }),
            "ir.module.module" => json!({"name": "crud_matrix_module"}),
            // no sequence exists to point at; the access check comes before the reference
            "ir.sequence.date_range" => json!({
                "sequence_id": NO_SUCH_ID, "date_from": "2026-01-01", "date_to": "2026-12-31"
            }),
            "ir.ui.view" => json!({
                "name": tag,
                "model": "res.partner",
                "priority": 99,
                "arch": "<form><field name=\"name\"/></form>",
            }),
            "report.job" => json!({
                "name": "crud-matrix.pdf",
                "user_id": self.fx.user(co.owner().unwrap_or(Who::Admin).subject()).id,
                "report_name": "crud.matrix",
                "res_model": "res.partner",
                "res_ids": "[1]",
            }),
            "res.country.group"
            | "res.groups"
            | "res.groups.privilege"
            | "res.partner.category"
            | "res.partner.industry" => json!({"name": tag}),
            "res.company" => json!({"name": tag, "currency_id": base.currency}),
            "res.country" => json!({"name": tag, "code": self.code()}),
            "res.country.state" => json!({
                "name": tag,
                "code": format!("S{}", self.next()),
                "country_id": self.make("res.country", &parent, Co::Shared),
            }),
            "res.currency" => json!({"name": format!("Z{}", self.code()), "symbol": "¤"}),
            "res.currency.rate" => {
                // one rate per currency, company, rate type and day: every payload takes its own day
                let n = self.next();
                let day = format!(
                    "{}-{:02}-{:02}",
                    2001 + n / 336,
                    1 + (n / 28) % 12,
                    1 + n % 28
                );
                json!({
                    "name": day,
                    "rate": 1.5,
                    "currency_id": base.currency,
                    "rate_type_id": base.rate_type,
                    "company_id": company,
                })
            }
            "res.currency.rate.type" => {
                json!({"name": tag, "code": format!("crud_{}", self.next())})
            }
            // A new language starts inactive, as a language waiting to be activated. Leaving
            // `active` out stores no value, and every later write of a translated field then fails
            // (finding MU-R02, task).
            "res.lang" => {
                let n = self.next();
                json!({
                    "name": tag,
                    "code": format!("xx_{n}"),
                    "url_code": format!("xx-{n}"),
                    "active": false,
                })
            }
            "res.partner" => json!({"name": tag, "company_id": company}),
            "res.users" => {
                // as the fixture provisions a user: its contact first, then the user in the main
                // company. The password is a disposable value of a user that never signs in.
                let main = self.company(Subject::Admin);
                let contact = self.create_id(
                    "res.partner",
                    json!({"name": parent, "company_id": main}),
                    &[],
                );
                let n = self.next();
                json!({
                    "name": tag,
                    "login": format!("crud-user-{n}"),
                    "password": format!("{tag} £ disposable {n}"),
                    "partner_id": contact,
                    "company_id": main,
                    "company_ids": [[6, 0, [main]]],
                    "group_ids": [[6, 0, [self.fx.group("group_user")]]],
                    "lang": "en_US",
                    "tz": "UTC",
                })
            }
            // a user has one settings row: every payload names a new user
            "res.users.settings" => {
                json!({"user_id": self.make("res.users", &parent, Co::Shared)})
            }
            other => panic!("no payload is written for {other}"),
        };
        // A shared record sends `null`, which JSON-2 binds to the empty value. An explicit `false`
        // was refused for product.combo.company_id (422, finding MU-C01, task;
        // other models not tried). Leaving the key out is
        // no substitute: product.uom then defaults to the active company and is not shared.
        // (a user's `company_id` is its default company, not a scope: it stays)
        if matches!(co, Co::Shared | Co::Admin)
            && model != "res.users"
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
            _ => self.create_id_as(
                Self::creator(model, co),
                model,
                self.payload(model, tag, co),
                &[],
            ),
        }
    }

    /// The first record of `model` the install ships (lowest identity), if any. For modules it is
    /// the row of `base`, which is installed: deleting an installed module row is the case that
    /// must not tell a caller without the grant anything about the row.
    fn shipped(&self, model: &str) -> Option<i64> {
        let domain = if model == "ir.module.module" {
            json!([["name", "=", "base"], ["state", "=", "installed"]])
        } else {
            json!([])
        };
        let found = self
            .prov_with(
                model,
                "search",
                json!({"domain": domain, "limit": 1, "order": "id"}),
                &[("active_test", json!(false))],
            )
            .unwrap_or_else(|e| panic!("shipped row lookup {e}"));
        ids_of(&found).and_then(|ids| ids.first().copied())
    }

    fn seed(&self, case: &ModelCase) -> Seeds {
        let tag = |suffix: &str| format!("crud {} seed {suffix}", case.model);
        match (case.scope, case.rows()) {
            (Scope::Global, Rows::Created) => Seeds {
                shared: Some(self.make(case.model, &tag("global"), Co::Shared)),
                ..Seeds::default()
            },
            (Scope::Global, Rows::Shipped) => Seeds {
                shared: Some(self.shipped(case.model).unwrap_or_else(|| {
                    panic!(
                        "{}: the manifest says the install ships a row, none exists",
                        case.model
                    )
                })),
                ..Seeds::default()
            },
            (Scope::Global, Rows::Absent | Rows::Volatile) => Seeds::default(),
            (Scope::Company, _) => Seeds {
                admin: None,
                a: Some(self.make(case.model, &tag("A"), Co::A)),
                b: Some(self.make(case.model, &tag("B"), Co::B)),
                shared: Some(self.make(case.model, &tag("shared"), Co::Shared)),
            },
            (Scope::Fixture, _) => {
                let of = |subject: Subject| match case.model {
                    "res.company" => self.company(subject),
                    "res.users" => self.fx.user(subject).id,
                    "res.users.settings" => self.settings_row(self.fx.user(subject).id),
                    other => panic!("no fixture-owned records are chosen for {other}"),
                };
                Seeds {
                    a: Some(of(Subject::A)),
                    b: Some(of(Subject::B)),
                    ..Seeds::default()
                }
            }
            (Scope::Owner, _) => Seeds {
                admin: Some(self.make(case.model, &tag("admin"), Co::Admin)),
                a: Some(self.make(case.model, &tag("A"), Co::A)),
                b: Some(self.make(case.model, &tag("B"), Co::B)),
                shared: matches!(case.policy, Policy::OwnRecords)
                    .then(|| self.make(case.model, &tag("shared"), Co::Shared)),
            },
        }
    }

    /// The settings row of `user`, created by the provisioning session when the user has none.
    fn settings_row(&self, user: i64) -> i64 {
        let model = "res.users.settings";
        let found = self
            .prov(model, "search", json!({"domain": [["user_id", "=", user]]}))
            .unwrap_or_else(|e| panic!("seed lookup {e}"));
        match ids_of(&found).as_deref() {
            Some([]) => self.create_id(model, json!({"user_id": user}), &[]),
            Some([id]) => *id,
            _ => panic!("seed lookup {model}: a user has more than one settings row"),
        }
    }

    fn count(&self, model: &str, domain: Value) -> i64 {
        self.count_as(None, model, domain)
    }

    fn count_as(&self, keeper: Option<Who>, model: &str, domain: Value) -> i64 {
        self.keep(
            keeper,
            model,
            "search_count",
            json!({"domain": domain}),
            &[("active_test", json!(false))],
        )
        .unwrap_or_else(|e| panic!("readback {e}"))
        .as_i64()
        .expect("search_count is an integer")
    }

    fn count_matrix_rows(&self, case: &ModelCase, keeper: Option<Who>) -> i64 {
        if case.model == "ir.approval.request" {
            // Raw access is closed to every RPC principal, including the provisioning user.
            // Check the durable state separately from the public access probes.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("matrix database observer runtime");
            return runtime
                .block_on(
                    self.fx
                        .native
                        .client
                        .query_one("SELECT count(*) FROM ir_approval_request", &[]),
                )
                .expect("approval request count through the fixture database owner")
                .get(0);
        }
        self.count_as(keeper, case.model, json!([]))
    }

    fn marker_value(&self, model: &str, id: i64, marker: &Marker) -> Value {
        self.marker_value_as(None, model, id, marker)
    }

    fn marker_value_as(&self, keeper: Option<Who>, model: &str, id: i64, marker: &Marker) -> Value {
        let rows = self
            .keep(
                keeper,
                model,
                "read",
                json!({"ids": [id], "fields": [marker.field()]}),
                &[],
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
        let all = seeds.all();
        // a model without any record is searched for an identity no fixture creates
        let domain = if all.is_empty() {
            json!([["id", "=", NO_SUCH_ID]])
        } else {
            json!([["id", "in", all]])
        };
        let out = self.search_all(who, case.model, domain);
        let mut details: Vec<String> = judge(
            case,
            Op::Search,
            expect,
            &out,
            self.record_subject(who),
            &[],
        )
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
                .into_iter()
                .chain(seeds.shared)
                .map(|id| (id, Expect::Ok))
                .chain(
                    seeds
                        .others(who)
                        .into_iter()
                        .map(|id| (id, Expect::RecordDenied)),
                )
                .collect(),
            other => {
                let all = seeds.all();
                if all.is_empty() {
                    vec![(NO_SUCH_ID, other)]
                } else {
                    all.into_iter().map(|id| (id, other)).collect()
                }
            }
        }
    }

    fn read_cell(&self, case: &ModelCase, who: Who, seeds: &Seeds) -> Vec<String> {
        let mut details = Vec::new();
        // An identity that names no record is read with a stored field, so the read has to look
        // the record up (the identity alone is answered without one).
        let field = match (seeds.all().is_empty(), Marker::of(case.model)) {
            (true, Some(marker)) => marker.field(),
            // a model without a field of its own still has the log columns
            (true, None) => "create_date",
            (false, _) => "id",
        };
        for (id, expect) in Self::read_targets(who, expect(case, who, Op::Read), seeds) {
            let body = json!({"ids": [id], "fields": [field]});
            let out = self.call(who, case.model, "read", body);
            let subject = self.record_subject(who);
            if let Some(detail) = judge(case, Op::Read, expect, &out, subject, &[id]) {
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
        let co = Self::co_of(who);
        let vals = self.payload(case.model, &tag, co);
        let reader = Self::reader(case.model, co);
        let before = self.count_matrix_rows(case, reader);
        let out = self.call(who, case.model, "create", json!({"vals_list": [vals]}));
        let after = self.count_matrix_rows(case, reader);
        let mut details: Vec<String> = judge(
            case,
            Op::Create,
            expect,
            &out,
            self.record_subject(who),
            &[],
        )
        .into_iter()
        .collect();
        if expect == Expect::Ok {
            let created = out.ids().and_then(|ids| ids.iter().next().copied());
            match created {
                Some(id) if self.count_as(reader, case.model, json!([["id", "=", id]])) == 1 => {}
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
        if let Some(expect) = expect_foreign(case, who, Op::Create) {
            // the same create, for another subject
            let (other, label) = Self::foreign_owners(case, who)[0];
            let vals = self.payload(case.model, &format!("{tag} for {label}"), other);
            let reader = Self::reader(case.model, other);
            let before = self.count_as(reader, case.model, json!([]));
            let out = self.call(who, case.model, "create", json!({"vals_list": [vals]}));
            let after = self.count_as(reader, case.model, json!([]));
            let subject = self.record_subject(who);
            if let Some(detail) = judge(case, Op::Create, expect, &out, subject, &[]) {
                details.push(format!("a record for {label}: {detail}"));
            }
            if after != before {
                details.push(format!(
                    "a record for {label}: its owner's record count went {before} -> {after}"
                ));
            }
        }
        Self::failures(case, who, Op::Create, details)
    }

    /// The record a write or unlink cell of `who` works on, and whose it is: a disposable record
    /// where records are created through RPC, the shipped row where nobody creates one, and an
    /// identity that names no record (`None` owner) where none exists.
    fn target(
        &self,
        case: &ModelCase,
        who: Who,
        expect: Expect,
        seeds: &Seeds,
        tag: &str,
    ) -> (i64, Option<Co>) {
        // where the policy grants exactly the own record, a success acts on it; a refusal always
        // gets a disposable record, so that a hole cannot remove one later cells need
        if case.acts_on_own_record()
            && expect == Expect::Ok
            && let Some(own) = seeds.own(who)
        {
            return (own, Some(Self::co_of(who)));
        }
        match (case.rows(), seeds.shared) {
            (Rows::Created, _) => {
                let co = Self::co_of(who);
                (self.make(case.model, tag, co), Some(co))
            }
            (Rows::Shipped, Some(id)) => (id, Some(Co::Shared)),
            (Rows::Shipped, None) => panic!("{}: no shipped row was seeded", case.model),
            (Rows::Absent | Rows::Volatile, _) => (NO_SUCH_ID, None),
        }
    }

    /// One write by `who` on record `id` kept by `co` (`None`: the identity names no record),
    /// with the readback.
    fn write_once(
        &self,
        case: &ModelCase,
        who: Who,
        expect: Expect,
        id: i64,
        co: Option<Co>,
        tag: &str,
    ) -> Vec<String> {
        let marker = Marker::of(case.model);
        let new = marker.as_ref().map(|marker| marker.new_value(tag));
        let vals = match (&marker, &new) {
            (Some(marker), Some(new)) => json!({marker.field(): new}),
            _ => json!({}),
        };
        let kept = co.zip(marker.as_ref());
        let read = |kept: Option<(Co, &Marker)>| {
            kept.map(|(co, marker)| {
                self.marker_value_as(Self::reader(case.model, co), case.model, id, marker)
            })
        };
        let before = read(kept);
        let body = json!({"ids": [id], "vals": vals});
        let out = self.call(who, case.model, "write", body);
        let after = read(kept);
        let mut details: Vec<String> = judge(
            case,
            Op::Write,
            expect,
            &out,
            self.record_subject(who),
            &[id],
        )
        .into_iter()
        .collect();
        if let (Some((_, marker)), Some(before), Some(after), Some(new)) =
            (kept, &before, &after, &new)
        {
            if expect == Expect::Ok {
                if !marker.same(after, new) {
                    details.push(format!("{} did not change after a write", marker.field()));
                }
            } else if !marker.same(after, before) {
                details.push(format!(
                    "{} changed although the write was refused",
                    marker.field()
                ));
            }
        }
        details
    }

    /// The other owners whose records `who` tries to create, change or delete in a user-owned
    /// model: another subject first, then nobody (a shared record) where the model has those.
    fn foreign_owners(case: &ModelCase, who: Who) -> Vec<(Co, &'static str)> {
        let other = match who {
            Who::A => (Co::B, "B"),
            Who::Admin | Who::B | Who::Anon => (Co::A, "A"),
        };
        let mut owners = vec![other];
        if matches!(case.policy, Policy::OwnRecords) {
            owners.push((Co::Shared, "nobody (shared)"));
        }
        owners
    }

    fn write_cell(&self, case: &ModelCase, who: Who, seeds: &Seeds) -> Vec<String> {
        let expect = expect(case, who, Op::Write);
        let tag = format!("crud {} {} write", case.model, who.label());
        let (id, co) = self.target(case, who, expect, seeds, &tag);
        let mut details = self.write_once(case, who, expect, id, co, &tag);
        if let Some(expect) = expect_foreign(case, who, Op::Write) {
            for (co, label) in Self::foreign_owners(case, who) {
                let tag = format!("{tag} on the record of {label}");
                let id = self.make(case.model, &tag, co);
                details.extend(
                    self.write_once(case, who, expect, id, Some(co), &tag)
                        .into_iter()
                        .map(|detail| format!("the record of {label}: {detail}")),
                );
            }
        }
        Self::failures(case, who, Op::Write, details)
    }

    /// One unlink by `who` of record `id` kept by `co`, with the readback.
    fn unlink_once(
        &self,
        case: &ModelCase,
        who: Who,
        expect: Expect,
        id: i64,
        co: Option<Co>,
    ) -> Vec<String> {
        let out = self.call(who, case.model, "unlink", json!({"ids": [id]}));
        let mut details: Vec<String> = judge(
            case,
            Op::Unlink,
            expect,
            &out,
            self.record_subject(who),
            &[id],
        )
        .into_iter()
        .collect();
        if let Some(co) = co {
            let reader = Self::reader(case.model, co);
            let left = self.count_as(reader, case.model, json!([["id", "=", id]]));
            let wanted = i64::from(expect != Expect::Ok);
            if left != wanted {
                details.push(format!("{left} record(s) remain, expected {wanted}"));
            }
        }
        details
    }

    fn unlink_cell(&self, case: &ModelCase, who: Who, seeds: &Seeds) -> Vec<String> {
        let expect = expect(case, who, Op::Unlink);
        let tag = format!("crud {} {} unlink", case.model, who.label());
        let (id, co) = self.target(case, who, expect, seeds, &tag);
        if let Some(defect) = known_defect(case, who, Op::Unlink) {
            let out = self.call(who, case.model, "unlink", json!({"ids": [id]}));
            let left = self.count(case.model, json!([["id", "=", id]]));
            let present = out.status == defect.status
                && out.name == defect.name
                && out.message == defect.message
                && left == 1;
            let detail = format!(
                "known defect of {} no longer reproduces (contract: {expect:?}); got {}, {left} record(s) remain: remove its KNOWN_DEFECTS entry",
                defect.task,
                out.brief()
            );
            return Self::failures(
                case,
                who,
                Op::Unlink,
                (!present).then_some(detail).into_iter().collect(),
            );
        }
        let mut details = self.unlink_once(case, who, expect, id, co);
        if let Some(expect) = expect_foreign(case, who, Op::Unlink) {
            for (co, label) in Self::foreign_owners(case, who) {
                let id = self.make(case.model, &format!("{tag} on the record of {label}"), co);
                details.extend(
                    self.unlink_once(case, who, expect, id, Some(co))
                        .into_iter()
                        .map(|detail| format!("the record of {label}: {detail}")),
                );
            }
        }
        Self::failures(case, who, Op::Unlink, details)
    }

    /// Every cell of one written model, with the checks its policy adds.
    fn model_cells(&self, case: &ModelCase) -> Vec<String> {
        let seeds = self.seed(case);
        let mut found = Vec::new();
        if case.rows() == Rows::Absent {
            // the manifest says no record exists; one that does would go untested
            let n = self.count_matrix_rows(case, None);
            if n != 0 {
                found.push(format!(
                    "[{}] the manifest says no record exists, the provisioning administrator finds {n}",
                    case.model
                ));
            }
        }
        if case.model == "ir.approval.request" {
            for who in [Who::Admin, Who::A, Who::B] {
                let out = self.call(who, case.model, "search_count", json!({"domain": []}));
                if let Some(detail) = judge(case, Op::Search, Expect::ModelDenied, &out, None, &[])
                {
                    found.push(Self::fail(
                        case,
                        who,
                        Op::Search,
                        &format!("search_count: {detail}"),
                    ));
                }
            }
        }
        // before the cells: an own-row unlink below removes a row these checks read
        match case.policy {
            Policy::OwnUser { .. } => found.extend(self.own_user_extras(case, &seeds)),
            Policy::OwnSettings { .. } => found.extend(self.own_settings_extras(case, &seeds)),
            _ => {}
        }
        // operation by operation: an own-row unlink comes after every principal's reads
        for op in OPS {
            for who in [Who::Admin, Who::A, Who::B] {
                found.extend(match op {
                    Op::Search => self.search_cell(case, who, &seeds),
                    Op::Read => self.read_cell(case, who, &seeds),
                    Op::Create => self.create_cell(case, who),
                    Op::Write => self.write_cell(case, who, &seeds),
                    Op::Unlink => self.unlink_cell(case, who, &seeds),
                });
            }
        }
        found
    }

    /// Every non-deferred cell of the manifest. Returns the mismatches and the deferred cell count.
    /// A seed or readback that cannot run (a payload defect, a server error) stops that model only
    /// and is reported with the other mismatches, so one model never hides the rest.
    pub fn family_matrix(&self) -> (Vec<String>, usize) {
        let mut mismatches = Vec::new();
        let mut deferred_cells = 0;
        for case in MANIFEST {
            if case.is_deferred() {
                deferred_cells += 3 * OPS.len();
                continue;
            }
            if case.policy == Policy::Specialized {
                continue;
            }
            mismatches.extend(Self::guarded(case.model, || self.model_cells(case)));
        }
        (mismatches, deferred_cells)
    }

    /// Run one group of checks; a panic inside it becomes one more mismatch line.
    pub fn guarded(label: &str, checks: impl FnOnce() -> Vec<String>) -> Vec<String> {
        match catch_unwind(AssertUnwindSafe(checks)) {
            Ok(found) => found,
            Err(panic) => vec![format!(
                "[{label}] stopped before its checks finished: {}",
                panic_text(panic.as_ref())
            )],
        }
    }

    /// One refused mutation of `id` by `who`, with the record checked unchanged or still present.
    fn refused(
        &self,
        case: &ModelCase,
        who: Who,
        op: Op,
        expect: Expect,
        id: i64,
        what: &str,
    ) -> Vec<String> {
        let marker = if let Expect::FieldDenied(field) = expect {
            Marker::Char(field)
        } else {
            Marker::of(case.model).expect("a model with refused mutations has a marker")
        };
        let before = self.marker_value(case.model, id, &marker);
        let body = match op {
            Op::Write => json!({"ids": [id], "vals": {marker.field(): marker.new_value(what)}}),
            Op::Unlink => json!({"ids": [id]}),
            Op::Search | Op::Read | Op::Create => panic!("refused() takes a write or an unlink"),
        };
        let out = self.call(who, case.model, op.method(), body);
        let mut details: Vec<String> =
            judge(case, op, expect, &out, self.record_subject(who), &[id])
                .into_iter()
                .collect();
        if self.count(case.model, json!([["id", "=", id]])) != 1 {
            details.push("the record is gone after a refusal".to_owned());
        } else if !marker.same(&self.marker_value(case.model, id, &marker), &before) {
            details.push(format!(
                "{} changed although the write was refused",
                marker.field()
            ));
        }
        let details = details
            .into_iter()
            .map(|detail| format!("{what}: {detail}"))
            .collect();
        Self::failures(case, who, op, details)
    }

    /// `res.users` beyond the cells: a user may not write its own
    /// login, nor any field of another user.
    fn own_user_extras(&self, case: &ModelCase, seeds: &Seeds) -> Vec<String> {
        let mut found = Vec::new();
        for who in [Who::A, Who::B] {
            let others = seeds.others(who);
            let (Some(own), [other]) = (seeds.own(who), others.as_slice()) else {
                panic!("res.users seeds lack a principal's record");
            };
            found.extend(self.refused(
                case,
                who,
                Op::Write,
                Expect::FieldDenied("login"),
                own,
                "own login",
            ));
            found.extend(self.refused(
                case,
                who,
                Op::Write,
                Expect::RecordDenied,
                *other,
                "another user's record",
            ));
        }
        found
    }

    /// `res.users.settings` beyond the cells: another user's row is neither written nor
    /// removed.
    fn own_settings_extras(&self, case: &ModelCase, seeds: &Seeds) -> Vec<String> {
        let mut found = Vec::new();
        for who in [Who::A, Who::B] {
            let others = seeds.others(who);
            let [other] = others.as_slice() else {
                panic!("res.users.settings seeds lack a principal's row");
            };
            for op in [Op::Write, Op::Unlink] {
                found.extend(self.refused(
                    case,
                    who,
                    op,
                    Expect::RecordDenied,
                    *other,
                    "another user's row",
                ));
            }
        }
        found
    }

    /// and reads the contact of B (company B), and B that of A.
    pub fn internal_contact_escape(&self) -> Vec<String> {
        let case = MANIFEST
            .iter()
            .find(|case| case.model == "res.partner")
            .expect("res.partner is in the manifest");
        let mut found = Vec::new();
        for (who, other) in [(Who::A, Subject::B), (Who::B, Subject::A)] {
            let user = self.fx.user(other).id;
            let rows = self
                .prov(
                    "res.users",
                    "read",
                    json!({"ids": [user], "fields": ["partner_id"]}),
                )
                .unwrap_or_else(|e| panic!("contact lookup {e}"));
            let link = &rows[0]["partner_id"];
            let contact = link
                .as_i64()
                .or_else(|| link.get(0).and_then(Value::as_i64))
                .expect("a user's contact identity");
            let search = self.call(
                who,
                case.model,
                "search",
                json!({"domain": [["id", "=", contact]]}),
            );
            let mut details = Vec::new();
            if search.ids() != Some(BTreeSet::from([contact])) {
                details.push(format!(
                    "search did not return the other internal user's contact ({})",
                    search.brief()
                ));
            }
            let read = self.call(
                who,
                case.model,
                "read",
                json!({"ids": [contact], "fields": ["id"]}),
            );
            if read.status != 200 {
                details.push(format!(
                    "read of the other internal user's contact: {}",
                    read.brief()
                ));
            }
            found.extend(Self::failures(case, who, Op::Read, details));
        }
        found
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
            let mut details: Vec<String> = judge(
                case,
                Op::Write,
                expect,
                &out,
                self.record_subject(who),
                &ids,
            )
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
            if let Some(detail) = judge(case, op, Expect::Unauthenticated, &out, None, &[]) {
                found.push(Self::fail(case, Who::Anon, op, &detail));
            }
        }
        found
    }
}
