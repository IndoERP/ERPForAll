use std::num::NonZeroU64;
use std::sync::{Arc, OnceLock};

use erp_domain::{Domain, DomainKind, Operator, PrincipalParam, Value};
use erp_fields::OnDelete;
use erp_orm::{
    AccessPolicy, CompanyId, GroupId, GroupSet, Principal, PrincipalBinding, PrincipalBuilder,
    QualifiedXmlId, Resolution, UserId,
};
use erp_registry::{
    AddonDescriptor, CompanyAccessDecl, FieldDecl, ModelDecl, ModelSpec, Registry, RegistryBuilder,
};

use crate::rows::{GroupRow, Implication, PolicyRows, RuleId, RuleRow};
use crate::snapshot::{Compiler, PolicySnapshot};

pub(crate) static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_access",
    depends: &["base"],
    exact_only: false,
};

pub(crate) const SOME_OBJ: &str = "test_access_right.some_obj";
pub(crate) const CATEG: &str = "test_access_right.obj_categ";
pub(crate) const CHILD: &str = "test_access_right.inherits";
pub(crate) const GRANDCHILD: &str = "test_access_right.grandchild";
/// Delegates to `some_obj` without its access (`_check_inherits_access = False`).
pub(crate) const UNCHECKED: &str = "test_access_right.unchecked";

pub(crate) static DECLS: [ModelDecl; 10] = [
    ModelDecl::new("test_access", "res.company")
        .description("Companies")
        .parent_name("parent_id")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("parent_id", "res.company"),
                ]
            },
        ),
    ModelDecl::new("test_access", "res.partner")
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests handwritten rules",
        ))
        .description("Contact")
        .parent_name("parent_id")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("parent_id", "res.partner"),
                    FieldDecl::one2many("child_ids", "res.partner", "parent_id"),
                    FieldDecl::many2one("commercial_partner_id", "res.partner"),
                    FieldDecl::many2one("company_id", "res.company"),
                ]
            },
        ),
    ModelDecl::extension("test_access", "res.users", &["res.users"])
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests handwritten rules",
        ))
        .fields(
            const {
                &[
                    FieldDecl::many2one("partner_id", "res.partner"),
                    FieldDecl::many2one("company_id", "res.company"),
                    FieldDecl::many2many("company_ids", "res.company"),
                    FieldDecl::one2many("employee_ids", "test.employee", "user_id"),
                    FieldDecl::many2one("employee_id", "test.employee"),
                ]
            },
        ),
    ModelDecl::new("test_access", "test.employee")
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests handwritten rules",
        ))
        .description("Employee")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("user_id", "res.users"),
                    FieldDecl::many2one("company_id", "res.company"),
                ]
            },
        ),
    ModelDecl::new("test_access", CATEG)
        .description("Object Category")
        .fields(const { &[FieldDecl::char("name"), FieldDecl::boolean("active")] }),
    ModelDecl::new("test_access", SOME_OBJ)
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests handwritten rules",
        ))
        .description("Object For Test Access Right")
        .parent_name("parent_id")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::integer("val"),
                    FieldDecl::many2one("categ_id", CATEG),
                    FieldDecl::many2many("tag_ids", CATEG),
                    FieldDecl::many2one("company_id", "res.company"),
                    FieldDecl::many2one("parent_id", SOME_OBJ),
                    FieldDecl::one2many("child_ids", SOME_OBJ, "parent_id"),
                    FieldDecl::many2one("partner_id", "res.partner"),
                    FieldDecl::many2one("user_id", "res.users"),
                    FieldDecl::date("day"),
                    FieldDecl::datetime("moment"),
                ]
            },
        ),
    ModelDecl::new("test_access", CHILD)
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests delegated access rules",
        ))
        .description("Object For Test Access Right Inherits")
        .inherits(&[(SOME_OBJ, "some_id")])
        .fields(
            const {
                &[
                    FieldDecl::many2one("some_id", SOME_OBJ)
                        .required(true)
                        .ondelete(OnDelete::Cascade),
                    FieldDecl::char("label"),
                ]
            },
        ),
    ModelDecl::new("test_access", GRANDCHILD)
        .description("Grandchild")
        .inherits(&[(CHILD, "child_id")])
        .fields(
            const {
                &[FieldDecl::many2one("child_id", CHILD)
                    .required(true)
                    .ondelete(OnDelete::Cascade)]
            },
        ),
    ModelDecl::new("test_access", UNCHECKED)
        .company_access(CompanyAccessDecl::unrestricted(
            "security fixture tests delegated access rules",
        ))
        .description("Unchecked Delegation")
        .inherits(&[(SOME_OBJ, "some_id")])
        .inherits_access(false)
        .fields(
            const {
                &[FieldDecl::many2one("some_id", SOME_OBJ)
                    .required(true)
                    .ondelete(OnDelete::Cascade)]
            },
        ),
    ModelDecl::new("test_access", "ir.access")
        .description("Access")
        .fields(const { &[FieldDecl::char("name")] }),
];

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

/// The fixture registry (composed once per test process).
pub(crate) fn registry() -> &'static Registry {
    registry_arc_ref()
}

fn registry_arc_ref() -> &'static Arc<Registry> {
    REGISTRY.get_or_init(|| {
        Arc::new(
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls(DECLS.iter())
                .build()
                .unwrap(),
        )
    })
}

/// The fixture registry, shared.
pub(crate) fn registry_arc() -> Arc<Registry> {
    Arc::clone(registry_arc_ref())
}

pub(crate) fn model(name: &str) -> &'static ModelSpec {
    registry().model(name).unwrap()
}

pub(crate) fn g(id: i64) -> GroupId {
    GroupId::new(id).unwrap()
}

pub(crate) fn xid(s: &str) -> QualifiedXmlId {
    QualifiedXmlId::parse(s).unwrap()
}

pub(crate) fn group(id: i64, name: &str) -> GroupRow {
    GroupRow::new(g(id), name).unwrap()
}

/// A rule `id` named `name` on `model` for `operation` (`crud` spelling), optionally of `group`
/// and with `domain`.
pub(crate) fn rule(
    id: i64,
    name: &str,
    model: &str,
    operation: &str,
    group: Option<i64>,
    domain: Option<&str>,
) -> RuleRow {
    let mut row = RuleRow::new(RuleId::new(id).unwrap(), name, model, operation).unwrap();
    if let Some(group) = group {
        row = row.with_group(g(group));
    }
    if let Some(domain) = domain {
        row = row.with_domain(domain);
    }
    row
}

/// `base.group_user` (implies everyone).
pub(crate) const USER: i64 = 1;
/// "Group 1".
pub(crate) const G1: i64 = 2;
/// "Group 2".
pub(crate) const G2: i64 = 3;
/// "Group 3".
pub(crate) const G3: i64 = 4;
/// `base.group_everyone`.
pub(crate) const EVERYONE: i64 = 5;
/// `base.group_portal` (a share group, implies everyone).
pub(crate) const PORTAL: i64 = 6;
/// Bob's user id.
pub(crate) const BOB: i64 = 7;
/// `base.group_no_one` (technical features, the debug listing).
pub(crate) const NO_ONE: i64 = 10;

/// Mario, Luigi, Peach, Toad, Yoshi, Bowser.
pub(crate) const ALL: [i64; 6] = [1, 2, 3, 4, 5, 6];

pub(crate) fn base_groups() -> Vec<GroupRow> {
    vec![
        group(USER, "Internal User").with_xmlid(xid("base.group_user")),
        group(G1, "Group 1"),
        group(G2, "Group 2"),
        group(G3, "Group 3"),
        group(EVERYONE, "Everyone").with_xmlid(xid("base.group_everyone")),
        group(PORTAL, "Portal")
            .with_xmlid(xid("base.group_portal"))
            .with_share(true),
        group(NO_ONE, "Technical Features").with_xmlid(xid("base.group_no_one")),
    ]
}

/// The fixture groups and implications with `rules`.
pub(crate) fn rows(rules: Vec<RuleRow>) -> PolicyRows {
    PolicyRows::builder(1)
        .groups(base_groups())
        .implications([
            Implication::new(g(USER), g(EVERYONE)),
            Implication::new(g(PORTAL), g(EVERYONE)),
        ])
        .rules(rules)
        .build()
        .unwrap()
}

pub(crate) fn snapshot(rules: Vec<RuleRow>) -> PolicySnapshot {
    Compiler::new(registry_arc()).compile(&rows(rules)).unwrap()
}

pub(crate) fn on(
    id: i64,
    name: &str,
    ids: Option<&[i64]>,
    group: Option<i64>,
    op: &str,
) -> RuleRow {
    let domain = ids.map(|ids| format!("[('id', 'in', {ids:?})]"));
    rule(id, name, SOME_OBJ, op, group, domain.as_deref())
}

/// A resolved user principal of `uid` in `direct` groups (closure from `policy`), companies 1
/// and 2 (current 1).
pub(crate) fn principal(policy: &dyn AccessPolicy, uid: i64, direct: &[i64]) -> Principal {
    principal_in(policy, uid, direct, false)
}

/// [`principal`] in debug mode or not.
pub(crate) fn principal_in(
    policy: &dyn AccessPolicy,
    uid: i64,
    direct: &[i64],
    debug: bool,
) -> Principal {
    let direct: Vec<GroupId> = direct.iter().map(|i| g(*i)).collect();
    let groups: GroupSet = policy.groups(&direct).unwrap();
    let companies = [CompanyId::new(1).unwrap(), CompanyId::new(2).unwrap()];
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(Resolution::in_id_order(groups, companies, NonZeroU64::MIN).unwrap())
        .companies(vec![companies[0]])
        .debug(debug)
        .build()
        .unwrap()
}

/// The binding the engine would build for `principal` under `policy`.
pub(crate) fn bind<'p>(
    policy: &dyn AccessPolicy,
    principal: &'p Principal,
) -> PrincipalBinding<'p> {
    PrincipalBinding::bind(policy, principal, Arc::clone(principal.resolution())).unwrap()
}

// ----- an in-memory oracle over a few records -------------------------------------------------

/// A field value of a test record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum V {
    Int(i64),
    Str(&'static str),
    /// A many2one to the record of that id.
    Ref(i64),
    Null,
}

/// Records of the fixture models, and the evaluation of a compiled predicate on them (a test
/// oracle for the conditions the fixtures use; anything else panics).
/// A record: its model, id and field values.
type Record = (&'static str, i64, Vec<(&'static str, V)>);

#[derive(Debug, Default, Clone)]
pub(crate) struct Db {
    records: Vec<Record>,
}

impl Db {
    pub(crate) fn with(
        mut self,
        model: &'static str,
        id: i64,
        fields: &[(&'static str, V)],
    ) -> Self {
        self.records.push((model, id, fields.to_vec()));
        self
    }

    pub(crate) fn set(&mut self, model: &str, id: i64, field: &'static str, value: V) {
        let record = self
            .records
            .iter_mut()
            .find(|(m, i, _)| *m == model && *i == id)
            .unwrap();
        record.2.retain(|(f, _)| *f != field);
        record.2.push((field, value));
    }

    fn value(&self, model: &str, id: i64, field: &str) -> V {
        if field == "id" {
            return V::Int(id);
        }
        let (_, _, fields) = self
            .records
            .iter()
            .find(|(m, i, _)| *m == model && *i == id)
            .unwrap_or_else(|| panic!("no record {model}({id})"));
        fields
            .iter()
            .find(|(f, _)| *f == field)
            .map_or(V::Null, |(_, v)| *v)
    }

    /// The ids of `model` among `ids` that satisfy `domain` for user `uid`.
    pub(crate) fn filter(&self, model: &str, ids: &[i64], domain: &Domain, uid: i64) -> Vec<i64> {
        ids.iter()
            .copied()
            .filter(|id| self.matches(model, *id, domain, uid))
            .collect()
    }

    pub(crate) fn matches(&self, model: &str, id: i64, domain: &Domain, uid: i64) -> bool {
        match domain.kind() {
            DomainKind::True => true,
            DomainKind::False => false,
            DomainKind::Not(child) => !self.matches(model, id, child, uid),
            DomainKind::And(children) => children.iter().all(|c| self.matches(model, id, c, uid)),
            DomainKind::Or(children) => children.iter().any(|c| self.matches(model, id, c, uid)),
            DomainKind::Condition(c) => {
                let actual = self.value(model, id, c.field_expr());
                match c.operator() {
                    Operator::AnyBypass | Operator::NotAnyBypass => {
                        let Value::Domain(sub) = c.value() else {
                            panic!("any! without a domain: {domain:?}")
                        };
                        let spec = crate::testing::model(model).field(c.field_expr()).unwrap();
                        let comodel = crate::analyze::comodel_of(&spec.ty).unwrap();
                        let found = match actual {
                            V::Ref(target) => self.matches(comodel, target, sub, uid),
                            V::Null => false,
                            other => panic!("any! on {other:?}"),
                        };
                        found == (c.operator() == Operator::AnyBypass)
                    }
                    Operator::In | Operator::Eq => self.member(actual, c.value(), uid),
                    Operator::NotIn | Operator::Ne => !self.member(actual, c.value(), uid),
                    Operator::Gt | Operator::Lt | Operator::Ge | Operator::Le => {
                        let (V::Int(a), Value::Int(b)) = (actual, c.value()) else {
                            return false;
                        };
                        match c.operator() {
                            Operator::Gt => a > *b,
                            Operator::Lt => a < *b,
                            Operator::Ge => a >= *b,
                            _ => a <= *b,
                        }
                    }
                    other => panic!("the oracle does not evaluate {other:?}: {domain:?}"),
                }
            }
            DomainKind::Custom(_) => panic!("custom node"),
        }
    }

    fn member(&self, actual: V, value: &Value, uid: i64) -> bool {
        let items: Vec<Value> = match value {
            Value::List(items) => items.clone(),
            Value::Set(set) => set.iter().cloned().collect(),
            other => vec![other.clone()],
        };
        items.iter().any(|item| match (actual, item) {
            (V::Int(a) | V::Ref(a), Value::Int(b)) => a == *b,
            (V::Int(a) | V::Ref(a), Value::Param(PrincipalParam::Uid)) => a == uid,
            (V::Str(a), Value::Str(b)) => a == b,
            (V::Null, Value::Bool(false)) => true,
            _ => false,
        })
    }
}

/// Mario (1), Luigi (2), Peach (3), Toad (4), Yoshi (5), Bowser (6) of `test_access_right.some_obj`.
pub(crate) fn objs() -> Db {
    let names = ["Mario", "Luigi", "Peach", "Toad", "Yoshi", "Bowser"];
    names.iter().zip(ALL).fold(Db::default(), |db, (name, id)| {
        db.with(SOME_OBJ, id, &[("name", V::Str(name)), ("val", V::Int(id))])
    })
}
