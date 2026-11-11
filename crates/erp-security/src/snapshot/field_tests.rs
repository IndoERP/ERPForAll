//! Field masks of the compiled policy: group specs on declared fields and on
//! custom fields, the memo, the debug detail of denials, `has_groups` and capabilities.

use std::sync::Arc;

use erp_fields::{ErrorKind, Result, Value};
use erp_orm::{
    AccessPolicy, Context, FieldAccessCtx, FieldAccessImpl, FieldAccessSuper, FieldContextValues,
    FieldDenialDetail, FieldMask, FieldOp, Principal,
};
use erp_registry::{
    AddonDescriptor, FieldDecl, FieldDef, KindDef, ManualModel, ModelDecl, ModelSpec, Overlay,
    Registry, RegistryBuilder,
};

use super::*;
use crate::report::{FieldGroupsProblem, GroupsSource};
use crate::rows::{
    CapabilityRow, FieldGroupsRow, Implication, PolicyRows, PrivilegeId, PrivilegeRow,
};
use crate::testing::{BOB, EVERYONE, NO_ONE, PORTAL, USER, bind, g, group, principal_in, xid};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_fgroups",
    depends: &["base"],
    exact_only: false,
};

const OBJ: &str = "test_access_right.some_obj";
const OPEN: &str = "test_access_right.open";
/// "Test Group" (`test_fgroups.group_test`).
const TEST_GROUP: i64 = 11;
/// A member of the test group and of nothing else.
const OTHER: i64 = 12;

static DECLS: [ModelDecl; 3] = [
    ModelDecl::new("test_fgroups", OBJ)
        .description("Object For Test Access Right")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::integer("val"),
                    FieldDecl::char("forbidden")
                        .groups("base.group_portal,test_fgroups.group_test"),
                    FieldDecl::char("forbidden2").groups("test_fgroups.group_test"),
                    FieldDecl::char("forbidden3").groups("."),
                    FieldDecl::char("not_portal").groups("!base.group_portal"),
                    FieldDecl::char("technical").groups("base.group_no_one"),
                    FieldDecl::char("broken").groups("test_fgroups.nobody,base.group_user"),
                    FieldDecl::char("narrowed"),
                    FieldDecl::char("both").groups("base.group_user"),
                ]
            },
        ),
    ModelDecl::new("test_fgroups", OPEN)
        .description("Open")
        .fields(const { &[FieldDecl::char("name")] }),
    ModelDecl::new("test_fgroups", "ir.access")
        .description("Access")
        .fields(const { &[FieldDecl::char("name")] }),
];

fn registry() -> Arc<Registry> {
    let mut overlay = Overlay::new();
    overlay
        .add_manual_model(ManualModel::new("x_things").description("Things"))
        .unwrap();
    for name in ["x_secret", "x_open", "x_both"] {
        overlay
            .add_manual_field(
                "x_things",
                FieldDef::new(name, KindDef::Char { size: None }),
            )
            .unwrap();
    }
    overlay
        .add_manual_field(OBJ, FieldDef::new("x_custom", KindDef::Char { size: None }))
        .unwrap();
    Arc::new(
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls(DECLS.iter())
            .overlay(&overlay)
            .build()
            .unwrap(),
    )
}

fn rows(
    extra: impl FnOnce(crate::rows::PolicyRowsBuilder) -> crate::rows::PolicyRowsBuilder,
) -> PolicyRows {
    let privilege = PrivilegeId::new(1).unwrap();
    let builder = PolicyRows::builder(1)
        .privilege(PrivilegeRow::new(privilege, "Role").unwrap())
        .groups([
            group(USER, "Internal User").with_xmlid(xid("base.group_user")),
            group(EVERYONE, "Everyone").with_xmlid(xid("base.group_everyone")),
            group(PORTAL, "Portal")
                .with_xmlid(xid("base.group_portal"))
                .with_privilege(privilege)
                .with_share(true),
            group(NO_ONE, "Technical Features").with_xmlid(xid("base.group_no_one")),
            group(TEST_GROUP, "Test Group").with_xmlid(xid("test_fgroups.group_test")),
        ])
        .implications([
            Implication::new(g(USER), g(EVERYONE)),
            Implication::new(g(PORTAL), g(EVERYONE)),
        ]);
    extra(builder).build().unwrap()
}

fn snapshot_with(rows: &PolicyRows, compiler: impl FnOnce(Compiler) -> Compiler) -> PolicySnapshot {
    compiler(Compiler::new(registry())).compile(rows).unwrap()
}

fn snapshot() -> PolicySnapshot {
    snapshot_with(&rows(|b| b), |c| c)
}

fn model_of<'p>(policy: &'p PolicySnapshot, name: &str) -> &'p ModelSpec {
    policy.registry().model(name).unwrap()
}

/// The names of the fields of `name` that `mask` lets `op` through.
fn allowed(
    policy: &PolicySnapshot,
    mask: &FieldMask,
    name: &str,
    op: FieldOp,
) -> Vec<&'static str> {
    model_of(policy, name)
        .fields
        .iter()
        .filter(|f| mask.allows(f.id, op))
        .map(|f| f.name)
        .collect()
}

fn mask_of(policy: &PolicySnapshot, p: &Principal, name: &str) -> Arc<FieldMask> {
    policy
        .field_access(&bind(policy, p), model_of(policy, name))
        .unwrap()
}

/// from a real database is `tests/sec023.rs`)
#[test]
fn group_specs_of_declared_fields_give_the_masks() {
    let policy = snapshot();
    let all = |m: &FieldMask, op| allowed(&policy, m, OBJ, op);
    // an internal user: portal-only fields closed, `!portal` open, the technical group needs debug
    let internal = principal_in(&policy, BOB, &[USER], false);
    let mask = mask_of(&policy, &internal, OBJ);
    let read = all(&mask, FieldOp::Read);
    for open in ["name", "val", "not_portal", "narrowed", "both"] {
        assert!(read.contains(&open), "{open} in {read:?}");
    }
    for closed in [
        "forbidden",
        "forbidden2",
        "forbidden3",
        "technical",
        "broken",
    ] {
        assert!(!read.contains(&closed), "{closed} in {read:?}");
    }
    assert_eq!(
        read,
        all(&mask, FieldOp::Write),
        "groups close both operations"
    );
    let spec = model_of(&policy, OBJ);
    assert!(mask.never(spec.field_id("forbidden3").unwrap()));
    assert!(!mask.never(spec.field_id("forbidden").unwrap()));
    // a portal user: `forbidden` through the portal group, not `not_portal`, not `both`
    let portal = principal_in(&policy, BOB, &[PORTAL], false);
    let read = all(&mask_of(&policy, &portal, OBJ), FieldOp::Read);
    assert!(read.contains(&"forbidden"));
    assert!(!read.contains(&"not_portal"));
    assert!(!read.contains(&"both"));
    // the test group opens `forbidden` and `forbidden2`
    let other = principal_in(&policy, OTHER, &[TEST_GROUP], false);
    let read = all(&mask_of(&policy, &other, OBJ), FieldOp::Read);
    assert!(read.contains(&"forbidden") && read.contains(&"forbidden2"));
    assert!(!read.contains(&"forbidden3"));
    // the technical group counts in debug mode only, and the debug flag is part of the key
    let tech = |debug| principal_in(&policy, BOB, &[USER, NO_ONE], debug);
    let quiet = mask_of(&policy, &tech(false), OBJ);
    let debug = mask_of(&policy, &tech(true), OBJ);
    assert!(!all(&quiet, FieldOp::Read).contains(&"technical"));
    assert!(all(&debug, FieldOp::Read).contains(&"technical"));
    // a model without a restricted field has the shared mask and no memo entry
    let open = mask_of(&policy, &internal, OPEN);
    assert!(Arc::ptr_eq(&open, &FieldMask::shared_all()));
}

#[test]
fn masks_are_memoized_per_group_set_and_bounded() {
    let policy = snapshot();
    let first = principal_in(&policy, BOB, &[USER], false);
    let again = principal_in(&policy, BOB + 1, &[USER], false);
    let a = mask_of(&policy, &first, OBJ);
    // another user with the same groups, class and debug shares the entry
    assert!(Arc::ptr_eq(&a, &mask_of(&policy, &again, OBJ)));
    assert!(Arc::ptr_eq(&a, &mask_of(&policy, &first, OBJ)));
    // other group sets do not
    let portal = principal_in(&policy, BOB, &[PORTAL], false);
    assert!(!Arc::ptr_eq(&a, &mask_of(&policy, &portal, OBJ)));
    assert_eq!(policy.mask_evictions(), 0);

    let small = snapshot_with(&rows(|b| b), |c| c.memo_capacity(2));
    for groups in [&[USER][..], &[PORTAL], &[TEST_GROUP], &[USER, NO_ONE]] {
        let p = principal_in(&small, BOB, groups, false);
        mask_of(&small, &p, OBJ);
    }
    assert!(small.mask_evictions() > 0);
    // an evicted entry is recomputed to an equal mask
    let p = principal_in(&small, BOB, &[USER], false);
    assert_eq!(*mask_of(&small, &p, OBJ), *mask_of(&small, &p, OBJ));
}

#[test]
fn a_group_spec_with_an_unknown_group_denies_its_field_and_is_reported() {
    let policy = snapshot();
    let report = policy.report();
    assert!(report.has_errors());
    let issues = report.field_group_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].to_string(),
        "field 'broken' on test_access_right.some_obj (declared groups): the group spec names \
         the unknown group 'test_fgroups.nobody' [denies the field]"
    );
    assert!(issues[0].denies());
    assert_eq!(issues[0].source(), GroupsSource::Registry);
    assert!(
        report
            .to_string()
            .contains("error: field 'broken' on test_access_right.some_obj")
    );
    // even a member of the group the spec also names is refused: the spec is not partly applied
    let internal = principal_in(&policy, BOB, &[USER], false);
    let mask = mask_of(&policy, &internal, OBJ);
    let broken = model_of(&policy, OBJ).field_id("broken").unwrap();
    assert!(!mask.readable(broken) && !mask.writable(broken));
    // the debug detail names the broken spec instead of guessing groups
    let tech = principal_in(&policy, BOB, &[USER, NO_ONE], true);
    let b = bind(&policy, &tech);
    let field = model_of(&policy, OBJ).field("broken").unwrap();
    let err = policy
        .field_denial_detail(&b, model_of(&policy, OBJ), field, FieldOp::Read)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the groups of field 'broken' on test_access_right.some_obj (declared groups) do not \
         compile: the group spec names the unknown group 'test_fgroups.nobody'"
    );
}

/// The debug lines of a field denial: the groups that would allow it with their full names, in id
/// order, `always forbidden`, `custom field access rules`; only for members of the technical
/// group.
#[test]
fn spec_sec_023_field_error_detail_names_the_groups() {
    let policy = snapshot();
    let spec = model_of(&policy, OBJ);
    let detail = |p: &Principal, field: &str| {
        let b = bind(&policy, p);
        policy
            .field_denial_detail(&b, spec, spec.field(field).unwrap(), FieldOp::Read)
            .unwrap()
    };
    let tech = principal_in(&policy, BOB, &[USER, NO_ONE], true);
    let detail_text = |field| detail(&tech, field).map(|d| d.text());
    assert_eq!(
        detail_text("forbidden").as_deref(),
        Some("allowed for groups 'Role / Portal', 'Test Group'")
    );
    assert_eq!(
        detail_text("forbidden3").as_deref(),
        Some("always forbidden")
    );
    assert_eq!(
        detail_text("not_portal").as_deref(),
        Some("allowed for groups '!Role / Portal'")
    );
    assert_eq!(
        detail_text("technical").as_deref(),
        Some("allowed for groups 'Technical Features'")
    );
    // no groups on the field: an override or a custom rule denied it
    assert_eq!(
        detail(&tech, "narrowed"),
        Some(FieldDenialDetail::CustomRules)
    );
    // an internal user without the technical group is told nothing
    let plain = principal_in(&policy, BOB, &[USER], true);
    assert_eq!(detail(&plain, "forbidden"), None);
}

#[test]
fn non_ascii_and_quoted_group_names_in_the_detail() {
    let rows = rows(|b| {
        b.group(group(20, "Gérant 経理").with_xmlid(xid("test_fgroups.gerant")))
            .group(group(21, "Bob's group").with_xmlid(xid("test_fgroups.bobs")))
    });
    let policy = snapshot_with(&rows, |c| c);
    let spec = model_of(&policy, OBJ);
    let overlay = snapshot_with(
        &PolicyRows::builder(1)
            .groups(rows.groups().iter().cloned())
            .privileges(rows.privileges().iter().cloned())
            .implications(rows.implied().iter().cloned())
            .field_group(FieldGroupsRow::new(OBJ, "x_custom", [g(20), g(21)]).unwrap())
            .build()
            .unwrap(),
        |c| c,
    );
    let tech = principal_in(&overlay, BOB, &[USER, NO_ONE], true);
    let b = bind(&overlay, &tech);
    let field = spec.field("x_custom").unwrap();
    let detail = overlay
        .field_denial_detail(&b, model_of(&overlay, OBJ), field, FieldOp::Read)
        .unwrap()
        .unwrap();
    assert_eq!(
        detail.text(),
        "allowed for groups 'Gérant 経理', \"Bob's group\""
    );
}

#[test]
fn custom_field_groups_come_from_the_overlay() {
    let rows = rows(|b| {
        b.field_groups([
            FieldGroupsRow::new("x_things", "x_secret", [g(TEST_GROUP), g(PORTAL)]).unwrap(),
            // a registry field with groups AND an overlay row: both must hold
            FieldGroupsRow::new(OBJ, "both", [g(TEST_GROUP)]).unwrap(),
            // dangling rows restrict nothing and are reported as build errors
            FieldGroupsRow::new("x_gone", "x_secret", [g(TEST_GROUP)]).unwrap(),
            FieldGroupsRow::new("x_things", "x_gone", [g(TEST_GROUP)]).unwrap(),
        ])
    });
    let policy = snapshot_with(&rows, |c| c);
    let things = |p: &Principal| {
        let mask = mask_of(&policy, p, "x_things");
        allowed(&policy, &mask, "x_things", FieldOp::Read)
    };
    let other = principal_in(&policy, OTHER, &[TEST_GROUP], false);
    let internal = principal_in(&policy, BOB, &[USER], false);
    let portal = principal_in(&policy, BOB, &[PORTAL], false);
    assert!(things(&other).contains(&"x_secret"));
    assert!(things(&portal).contains(&"x_secret"));
    assert!(!things(&internal).contains(&"x_secret"));
    assert!(things(&internal).contains(&"x_open"));
    // the registry field also named by an overlay row needs the group of each
    let member = principal_in(&policy, BOB, &[USER, TEST_GROUP], false);
    let read = |p: &Principal| allowed(&policy, &mask_of(&policy, p, OBJ), OBJ, FieldOp::Read);
    assert!(read(&member).contains(&"both"));
    assert!(!read(&internal).contains(&"both"));
    assert!(!read(&other).contains(&"both"), "not internal");
    // the report
    let issues: Vec<String> = policy
        .report()
        .field_group_issues()
        .iter()
        .filter(|i| i.source() == GroupsSource::Overlay)
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        issues,
        [
            "custom field groups for 'x_secret' on x_gone: the model does not exist",
            "custom field groups for 'x_gone' on x_things: the model has no such field",
        ]
    );
    assert!(
        policy
            .report()
            .field_group_issues()
            .iter()
            .any(|i| matches!(i.problem(), FieldGroupsProblem::UnknownField))
    );
    // a dangling row alone is an error of the build (it restricts nothing: a typo would leave
    // the field readable), though it denies nothing
    let dangling = snapshot_with(
        &self::rows(|b| b.field_group(FieldGroupsRow::new("x_gone", "x", [g(USER)]).unwrap())),
        |c| c,
    );
    assert!(dangling.report().has_errors());
    let overlay: Vec<_> = dangling
        .report()
        .field_group_issues()
        .iter()
        .filter(|i| i.source() == GroupsSource::Overlay)
        .collect();
    assert_eq!(overlay.len(), 1, "{overlay:?}");
    assert!(!overlay[0].denies());
    assert!(
        dangling
            .report()
            .to_string()
            .contains("error: custom field groups for 'x'")
    );
}

/// Narrows `narrowed` for non-members of the test group and `val` while `fa_freeze` is set.
fn narrow(
    ctx: &FieldAccessCtx<'_>,
    op: FieldOp,
    mask: FieldMask,
    sup: FieldAccessSuper<'_>,
) -> Result<FieldMask> {
    let mut mask = sup.call(ctx, op, mask)?;
    if !ctx.has_group("test_fgroups.group_test")? {
        mask.deny(ctx.field("narrowed")?, op)?;
    }
    if op == FieldOp::Write && ctx.context("fa_freeze")? == Some(&Value::Bool(true)) {
        mask.deny(ctx.field("val")?, FieldOp::Write)?;
    }
    Ok(mask)
}

fn widen(
    ctx: &FieldAccessCtx<'_>,
    op: FieldOp,
    mask: FieldMask,
    sup: FieldAccessSuper<'_>,
) -> Result<FieldMask> {
    let mut mask = sup.call(ctx, op, mask)?;
    mask.grant(ctx.field("forbidden2")?, op)?;
    Ok(mask)
}

static NARROW: FieldAccessImpl = FieldAccessImpl {
    addon: "test_fgroups",
    model: OBJ,
    method: "narrow",
    context: &["fa_freeze"],
    run: narrow,
    file: file!(),
    line: line!(),
};

static WIDEN: FieldAccessImpl = FieldAccessImpl {
    addon: "test_fgroups",
    model: OBJ,
    method: "widen",
    context: &[],
    run: widen,
    file: file!(),
    line: line!(),
};

fn freeze(policy: &PolicySnapshot, p: &Principal, on: bool) -> Arc<FieldMask> {
    let context = Context::from_pairs([("fa_freeze", Value::Bool(on))]);
    let keys = ["fa_freeze"];
    let b = bind(policy, p).with_field_ctx(FieldContextValues::read(&keys, &context));
    policy.field_access(&b, model_of(policy, OBJ)).unwrap()
}

/// compiled snapshot; the engine's checks are `erp-test-models`)
#[test]
fn field_access_overrides_narrow_the_group_masks_and_are_memoized_by_declared_values() {
    let policy = snapshot_with(&rows(|b| b), |c| c.field_overrides([&NARROW]));
    let spec = model_of(&policy, OBJ);
    let (narrowed, val) = (
        spec.field_id("narrowed").unwrap(),
        spec.field_id("val").unwrap(),
    );
    let internal = principal_in(&policy, BOB, &[USER], false);
    let member = principal_in(&policy, OTHER, &[TEST_GROUP], false);
    let mask = freeze(&policy, &internal, false);
    assert!(!mask.readable(narrowed) && !mask.writable(narrowed));
    assert!(mask.writable(val));
    assert!(
        mask.readable(spec.field_id("not_portal").unwrap()),
        "groups still apply"
    );
    assert!(!mask.readable(spec.field_id("forbidden").unwrap()));
    assert!(freeze(&policy, &member, false).readable(narrowed));
    // the declared context value is part of the key
    let frozen = freeze(&policy, &internal, true);
    assert!(mask.writable(val) && !frozen.writable(val) && frozen.readable(val));
    assert!(Arc::ptr_eq(&frozen, &freeze(&policy, &internal, true)));
    assert!(!Arc::ptr_eq(&frozen, &freeze(&policy, &internal, false)));
    // a model without overrides ignores the declared values
    let open = |on| {
        let context = Context::from_pairs([("fa_freeze", Value::Bool(on))]);
        let b = bind(&policy, &internal)
            .with_field_ctx(FieldContextValues::read(&["fa_freeze"], &context));
        policy.field_access(&b, model_of(&policy, OPEN)).unwrap()
    };
    assert!(Arc::ptr_eq(&open(true), &open(false)));
}

#[test]
fn an_override_that_widens_a_group_mask_is_a_programming_error() {
    let policy = snapshot_with(&rows(|b| b), |c| c.field_overrides([&WIDEN]));
    let internal = principal_in(&policy, BOB, &[USER], false);
    let err = policy
        .field_access(&bind(&policy, &internal), model_of(&policy, OBJ))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field access override test_access_right.some_obj.widen widened field 'forbidden2'"
    );
}

#[test]
fn has_groups_is_evaluated_on_the_bindings_groups_without_statements() {
    let policy = snapshot();
    let internal = principal_in(&policy, BOB, &[USER], false);
    let b = bind(&policy, &internal);
    assert!(policy.has_groups(&b, "base.group_user").unwrap());
    // implied groups count
    assert!(policy.has_groups(&b, "base.group_everyone").unwrap());
    assert!(!policy.has_groups(&b, "base.group_portal").unwrap());
    assert!(
        policy
            .has_groups(&b, "base.group_user,!base.group_portal")
            .unwrap()
    );
    let err = policy
        .has_groups(&b, "base.group_user,nope.group")
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the group spec names the unknown group 'nope.group'"
    );
    // the technical group needs the debug flag of the principal
    let tech = |debug| principal_in(&policy, BOB, &[USER, NO_ONE], debug);
    assert!(
        !policy
            .has_groups(&bind(&policy, &tech(false)), "base.group_no_one")
            .unwrap()
    );
    assert!(
        policy
            .has_groups(&bind(&policy, &tech(true)), "base.group_no_one")
            .unwrap()
    );
}

static CAP_APPROVE: Capability = Capability::__declare("test_fgroups.approve", "tests");

#[test]
fn capabilities_are_the_mapped_group_spec_of_the_subject() {
    static LOOSE: Capability = Capability::__declare("test_fgroups.loose", "tests");
    static BROKEN: Capability = Capability::__declare("test_fgroups.broken", "tests");
    static UNREGISTERED: Capability = Capability::__declare("test_fgroups.nowhere", "tests");
    let rows = rows(|b| {
        b.capabilities([
            CapabilityRow::new("test_fgroups.approve", "base.group_user,!base.group_portal")
                .unwrap(),
            CapabilityRow::new("test_fgroups.loose", "!test_fgroups.group_test").unwrap(),
            CapabilityRow::new("test_fgroups.broken", "base.group_user,gone.group").unwrap(),
        ])
    });
    let policy = snapshot_with(&rows, |c| c.capabilities([&CAP_APPROVE, &LOOSE, &BROKEN]));
    let internal = principal_in(&policy, BOB, &[USER], false);
    let portal = principal_in(&policy, BOB, &[PORTAL], false);
    let member = principal_in(&policy, OTHER, &[TEST_GROUP], false);
    let has = |p: &Principal, cap: &Capability| policy.has_capability(&bind(&policy, p), cap);
    assert!(has(&internal, &CAP_APPROVE).unwrap());
    assert!(!has(&portal, &CAP_APPROVE).unwrap());
    assert!(!has(&member, &CAP_APPROVE).unwrap());
    // `!g` alone: everyone outside g
    assert!(has(&internal, &LOOSE).unwrap());
    assert!(!has(&member, &LOOSE).unwrap());
    // a spec that does not compile fails the check by name and is in the report
    let err = has(&internal, &BROKEN).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The capability 'test_fgroups.broken' cannot be evaluated: the group spec names the \
         unknown group 'gone.group'"
    );
    let issues = policy.report().capability_errors();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].to_string(),
        "capability 'test_fgroups.broken': the group spec names the unknown group 'gone.group' \
         [asking for it fails]"
    );
    assert!(policy.report().has_errors());
    // a value `capability!` never registered is refused, mapped or not
    let err = has(&internal, &UNREGISTERED).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    // a registered capability without a row is an error, never `false`
    let policy = snapshot_with(&self::rows(|b| b), |c| c.capabilities([&CAP_APPROVE]));
    let err = policy
        .has_capability(&bind(&policy, &internal), &CAP_APPROVE)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the capability 'test_fgroups.approve' has no ir_capability row in this policy: map it \
         to groups (the build report lists it)"
    );
    // (the other test of this binary registers `test_access.explain` with `capability!`)
    assert!(
        policy
            .report()
            .missing_capabilities()
            .contains(&"test_fgroups.approve")
    );
}

#[test]
fn the_closure_of_a_deleted_group_is_an_access_error_and_implied_groups_are_added() {
    let policy = snapshot();
    let closed = policy.groups(&[g(PORTAL), g(PORTAL)]).unwrap();
    assert_eq!(closed.ids(), [g(EVERYONE), g(PORTAL)]);
    let err = policy.groups(&[g(USER), g(9999)]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The group 9999 does not exist (any more): the user's groups cannot be resolved."
    );
}
