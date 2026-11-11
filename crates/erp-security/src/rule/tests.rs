//! Rule compilation: the symbolic evaluation table, validation and the errors.

use erp_domain::{Domain, NowKind, Operation, Operator, PrincipalParam, UserPath, Value};
use erp_fields::ErrorKind;
use erp_registry::{FieldId, ModelId};

use super::*;
use crate::testing::{CATEG, CHILD, SOME_OBJ, model, registry};

fn compile_on(model_name: &str, text: &str) -> Result<RuleDomain, RuleError> {
    compile(registry(), model(model_name), Some(text))
}

fn ok(text: &str) -> RuleDomain {
    compile_on(SOME_OBJ, text).unwrap()
}

fn err(text: &str) -> RuleError {
    compile_on(SOME_OBJ, text).unwrap_err()
}

fn cond(field: &str, op: Operator, value: impl Into<Value>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn param(p: PrincipalParam) -> Value {
    Value::Param(p)
}

fn path(p: &str) -> PrincipalParam {
    PrincipalParam::UserPath(UserPath::parse(p).unwrap())
}

fn field(model_name: &str, field: &str) -> (ModelId, FieldId) {
    let spec = model(model_name);
    (spec.id, spec.field_id(field).unwrap())
}

/// The models a compiled rule sub-selects.
fn subselects(rule: &RuleDomain, model_name: &str) -> Vec<ModelId> {
    crate::analyze::analyze(registry(), model(model_name), &rule.domain)
        .unwrap()
        .subselects
}

fn unsupported_message(err: &RuleError) -> String {
    assert!(
        matches!(err, RuleError::Unsupported { .. }),
        "expected Unsupported, got {err:?}"
    );
    err.to_string()
}

#[test]
fn no_domain_blank_domain_and_constants() {
    for text in [None, Some(""), Some("  \r\n\t ")] {
        let rule = compile(registry(), model(SOME_OBJ), text).unwrap();
        assert!(rule.domain.is_true(), "{text:?}");
        assert!(rule.reads.is_empty() && rule.access.is_empty() && !rule.mentions_company);
    }
    assert!(ok("[]").domain.is_true());
    assert!(ok("True").domain.is_true());
    assert!(ok("False").domain.is_false());
    assert!(ok("[(1, '=', 1)]").domain.is_true());
    assert!(ok("[(0, '=', 1)]").domain.is_false());
    // Python equality: True == 1
    assert!(ok("[(True, '=', 1)]").domain.is_true());
}

#[test]
fn literal_conditions_read_their_fields() {
    let rule = ok("\r\n[('val', '>', 0), ('name', '=', 'Gérant 経理')]\r\n");
    assert_eq!(
        rule.domain,
        Domain::and([
            cond("val", Operator::Gt, 0),
            cond("name", Operator::Eq, "Gérant 経理"),
        ])
    );
    assert_eq!(
        rule.reads,
        [field(SOME_OBJ, "val"), field(SOME_OBJ, "name")]
    );
    assert!(subselects(&rule, SOME_OBJ).is_empty());
    let rule = ok("['|', ('val', '=', -1), '!', ('val', 'in', (1, 2.5, None))]");
    assert_eq!(
        rule.domain,
        Domain::or([
            cond("val", Operator::Eq, -1),
            !cond(
                "val",
                Operator::In,
                Value::List(vec![Value::Int(1), Value::Float(2.5), Value::FALSE]),
            ),
        ])
    );
    let rule = ok("[('val', '=', -9223372036854775807)]");
    assert_eq!(
        rule.domain,
        cond("val", Operator::Eq, -9_223_372_036_854_775_807_i64)
    );
}

#[test]
fn user_symbols_become_parameters() {
    let rule = ok("[('user_id', '=', user.id)]");
    assert_eq!(
        rule.domain,
        cond("user_id", Operator::Eq, param(PrincipalParam::Uid))
    );
    assert!(
        subselects(&rule, SOME_OBJ).is_empty(),
        "user.id needs no sub-select"
    );
    // the record itself and `.ids` are the list of its ids
    let uid_list = Value::List(vec![param(PrincipalParam::Uid)]);
    assert_eq!(
        ok("[('user_id', '=', user)]").domain,
        cond("user_id", Operator::Eq, uid_list.clone())
    );
    assert_eq!(
        ok("[('user_id', 'in', user.ids)]").domain,
        cond("user_id", Operator::In, uid_list)
    );
}

#[test]
fn user_paths_keep_their_terminal_and_read_every_field() {
    let rule = ok("[('partner_id', 'child_of', user.partner_id.commercial_partner_id.id)]");
    assert_eq!(
        rule.domain,
        cond(
            "partner_id",
            Operator::ChildOf,
            param(path("partner_id.commercial_partner_id.id")),
        )
    );
    assert_eq!(
        rule.reads,
        [
            field(SOME_OBJ, "partner_id"),
            field("res.users", "partner_id"),
            field("res.partner", "commercial_partner_id"),
        ]
    );
    let partner = model("res.partner").id;
    let users = model("res.users").id;
    assert_eq!(subselects(&rule, SOME_OBJ), [partner, users]);
    // `.ids` (and a record value) are the ids: none when a link on the path is empty
    assert_eq!(
        ok("[('user_id', 'in', user.employee_id.user_id.ids)]").domain,
        cond(
            "user_id",
            Operator::In,
            param(path("employee_id.user_id.ids"))
        )
    );
    assert_eq!(
        ok("[('partner_id', 'in', user.partner_id)]").domain,
        cond("partner_id", Operator::In, param(path("partner_id.ids")))
    );
    assert_eq!(
        ok("[('user_id', 'in', user.employee_ids.user_id.ids)]").domain,
        cond(
            "user_id",
            Operator::In,
            param(path("employee_ids.user_id.ids"))
        )
    );
}

#[test]
fn user_paths_are_validated() {
    let e = err("[('user_id', '=', user.employee_ids.user_id.id)]");
    assert_eq!(
        unsupported_message(&e),
        "'user.employee_ids.user_id.id' is not supported in access rules: '.id' of a path \
         through a one2many or many2many field names several records: use '.ids'"
    );
    let e = err("[('name', '=', user.name)]");
    assert_eq!(
        unsupported_message(&e),
        "'user.name' is not supported in access rules: user.name is a value of the user's \
         record, not a record id: rules compare ids (user.<path>.id or .ids)"
    );
    let e = err("[('name', '=', user.login.x)]");
    assert_eq!(
        unsupported_message(&e),
        "'user.login.x' is not supported in access rules: 'login' of res.users is not a \
         relational field, so the path cannot continue"
    );
    let e = err("[('user_id', '=', user.nope.id)]");
    assert_eq!(
        e,
        RuleError::Invalid {
            message: "AttributeError: 'res.users' object has no attribute 'nope' (in \
                      user.nope.id)"
                .to_owned()
        }
    );
}

#[test]
fn company_symbols_and_list_concatenation() {
    let rule = ok("['|', ('company_id', '=', False), ('company_id', 'in', company_ids)]");
    assert_eq!(
        rule.domain,
        Domain::or([
            cond("company_id", Operator::Eq, false),
            cond(
                "company_id",
                Operator::In,
                param(PrincipalParam::CompanyIds)
            ),
        ])
    );
    assert!(rule.mentions_company);
    // 76 Community rules write `company_ids + [False]`
    let rule = ok("[('company_id', 'in', company_ids + [False])]");
    assert_eq!(
        rule.domain,
        cond(
            "company_id",
            Operator::In,
            Value::List(vec![param(PrincipalParam::CompanyIds), Value::FALSE]),
        )
    );
    let rule = ok("[('user_id', 'in', [False] + user.employee_ids.user_id.ids + [3])]");
    assert_eq!(
        rule.domain,
        cond(
            "user_id",
            Operator::In,
            Value::List(vec![
                Value::FALSE,
                param(path("employee_ids.user_id.ids")),
                Value::Int(3)
            ]),
        )
    );
    assert_eq!(
        ok("[('company_id', '=', company_id)]").domain,
        cond("company_id", Operator::Eq, param(PrincipalParam::CompanyId))
    );
    let e = err("[('company_id', 'in', company_ids + (False,))]");
    assert_eq!(
        e.to_string(),
        "TypeError: can only concatenate list to list and tuple to tuple (in company_ids + \
         (False,))"
    );
    let e = err("[('user_id', 'in', user.id + [False])]");
    assert_eq!(
        unsupported_message(&e),
        "'user.id + [False]' is not supported in access rules: '+' is compiled only to join \
         lists (company_ids + [False]); computations on values are not"
    );
    let e = err("[('val', '=', 1 + 1)]");
    assert_eq!(
        unsupported_message(&e),
        "'1 + 1' is not supported in access rules: '+' is compiled only to join lists \
         (company_ids + [False]); computations on values are not"
    );
}

#[test]
fn now_forms_are_typed_parameters() {
    let date = param(PrincipalParam::Now(NowKind::Date));
    let stamp = param(PrincipalParam::Now(NowKind::Timestamp));
    for (text, value) in [
        ("[('day', '<=', time.strftime('%Y-%m-%d'))]", &date),
        ("[('day', '<=', fields.Date.today())]", &date),
        ("[('day', '<=', datetime.date.today())]", &date),
        ("[('day', '<=', context_today())]", &date),
        (
            "[('moment', '<=', time.strftime('%Y-%m-%d %H:%M:%S'))]",
            &stamp,
        ),
        ("[('moment', '<=', datetime.now())]", &stamp),
        ("[('moment', '<=', datetime.datetime.now())]", &stamp),
        ("[('moment', '<=', fields.Datetime.now())]", &stamp),
    ] {
        let rule = ok(text);
        let field_name = if value == &date { "day" } else { "moment" };
        assert_eq!(
            rule.domain,
            cond(field_name, Operator::Le, value.clone()),
            "{text}"
        );
    }
    let e = err("[('day', '<', time.strftime('%Y-%m-01'))]");
    assert_eq!(
        unsupported_message(&e),
        "'time.strftime('%Y-%m-01')' is not supported in access rules: time.strftime is \
         compiled for the formats '%Y-%m-%d' (today) and '%Y-%m-%d %H:%M:%S' (now) only"
    );
    let e = err("[('moment', '>', datetime.now() - timedelta(days=1))]");
    assert_eq!(
        unsupported_message(&e),
        "'datetime.now() - timedelta(days=1)' is not supported in access rules: rules are lists \
         of conditions on literal values and principal symbols; computations are not compiled to \
         SQL"
    );
}

#[test]
fn basic_optimization_validates_parameters() {
    let invalid = |text: &str| match err(text) {
        RuleError::Invalid { message } => message,
        other => panic!("expected Invalid, got {other:?}"),
    };
    assert_eq!(
        invalid("[('day', '<=', datetime.now())]"),
        "The principal parameter datetime.datetime.now() cannot be used here: a date field \
         compares with context_today() in condition ('day', '<=', datetime.datetime.now())"
    );
    assert_eq!(
        invalid("[('moment', '<=', time.strftime('%Y-%m-%d'))]"),
        "The principal parameter context_today() cannot be used here: a datetime field compares \
         with datetime.datetime.now() in condition ('moment', '<=', context_today())"
    );
    assert_eq!(
        invalid("[('name', 'like', user.id)]"),
        "The principal parameter user.id cannot be used here: a pattern must be a literal string \
         in condition ('name', 'like', user.id)"
    );
    // a parameter where a literal works is fine
    ok("[('val', 'in', [user.id, 3])]");
}

#[test]
fn has_group_and_other_calls_are_refused_with_the_rewrite() {
    let e = err("[('val', '>', 0)] if user.has_group('base.group_user') else []");
    assert_eq!(
        unsupported_message(&e),
        "'[('val', '>', 0)] if user.has_group('base.group_user') else []' is not supported in \
         access rules: rules are lists of conditions on literal values and principal symbols; \
         computations are not compiled to SQL"
    );
    let e = err("[('user_id', '=', user.has_group('base.group_user'))]");
    assert_eq!(
        unsupported_message(&e),
        "'user.has_group('base.group_user')' is not supported in access rules: group checks \
         belong in the rule's group: make it a permission of that group"
    );
    let e = err("[('partner_id', '=', user.env.ref('base.main_partner').id)]");
    assert_eq!(
        unsupported_message(&e),
        "'user.env.ref('base.main_partner').id' is not supported in access rules: only \
         attributes of user (a path of relational fields ending in .id or .ids) are compiled"
    );
    let e = err("[('company_id', 'in', user.env.companies.ids)]");
    assert_eq!(
        unsupported_message(&e),
        "'user.env.companies.ids' is not supported in access rules: user.env is available only \
         as user.env['<model>'].search(<literal domain>)"
    );
    let e = err("[('website_id', '=', website.id)]");
    assert_eq!(
        unsupported_message(&e),
        "'website.id' is not supported in access rules: a rule may use literals and the names \
         user, company_ids, company_id and time"
    );
}

/// Compile half: `user.env[...].search([])` becomes a sub-query of literal
/// conditions only, so no context reaches the rule. The search results over the database are
/// `spec_sec_015_context_does_not_affect_access_domains` (`tests/spec_whole.rs`), the whole case.
#[test]
fn searches_become_sub_queries_with_the_active_filter() {
    let rule = ok("[('categ_id', 'in', user.env['test_access_right.obj_categ'].search([]).ids)]");
    let active = cond("active", Operator::In, Value::list([true]));
    assert_eq!(
        rule.domain,
        cond(
            "categ_id",
            Operator::AnyBypass,
            Value::Domain(active.clone())
        )
    );
    assert_eq!(
        rule.reads,
        [field(SOME_OBJ, "categ_id"), field(CATEG, "active")]
    );
    assert_eq!(subselects(&rule, SOME_OBJ), [model(CATEG).id]);
    // the active filter is not added when the search names the active field; `not in` is
    // `not any!`; without `.ids` the recordset means its ids
    let rule = ok(
        "[('tag_ids', 'not in', user.env['test_access_right.obj_categ'].search([('active', \
         'in', [True, False]), ('name', '=', 'x')]))]",
    );
    assert_eq!(
        rule.domain,
        cond(
            "tag_ids",
            Operator::NotAnyBypass,
            Value::Domain(Domain::and([
                cond("active", Operator::In, Value::list([true, false])),
                cond("name", Operator::Eq, "x"),
            ])),
        )
    );
    let e = err("[('partner_id', 'in', user.env['test_access_right.obj_categ'].search([]).ids)]");
    assert_eq!(
        unsupported_message(&e),
        "'('partner_id', 'in', user.env['test_access_right.obj_categ'].search([]))' is not \
         supported in access rules: test_access_right.some_obj.partner_id does not hold ids of \
         test_access_right.obj_categ, so the search cannot become a sub-query"
    );
    let e = err(
        "[('categ_id', 'in', user.env['test_access_right.obj_categ'].search([('name', '=', \
         user.partner_id.name)]).ids)]",
    );
    assert!(matches!(e, RuleError::Unsupported { .. }), "{e:?}");
    let e = err(
        "[('categ_id', 'in', user.env['test_access_right.obj_categ'].search([('name', '=', \
         company_id)]).ids)]",
    );
    assert_eq!(
        unsupported_message(&e),
        "'user.env['test_access_right.obj_categ'].search([('name', '=', company_id)])' is not \
         supported in access rules: the domain of a search in a rule must be literal (no user, \
         company or time symbols)"
    );
    let e = err("[('categ_id', 'in', user.env['nope.model'].search([]).ids)]");
    assert_eq!(
        e.to_string(),
        "KeyError: 'nope.model' (no such model) in condition ('categ_id', 'in', \
         user.env['nope.model'].search([]))"
    );
    let e = err("[('categ_id', '>', user.env['test_access_right.obj_categ'].search([]).ids)]");
    assert!(matches!(e, RuleError::Unsupported { .. }), "{e:?}");
}

#[test]
fn traversals_become_any_bypass() {
    let rule = ok("[('categ_id.name', '=', 'x'), ('tag_ids', 'not any', [('name', '=', 'y')])]");
    assert_eq!(
        rule.domain,
        Domain::and([
            cond(
                "categ_id",
                Operator::AnyBypass,
                Value::Domain(cond("name", Operator::Eq, "x")),
            ),
            cond(
                "tag_ids",
                Operator::NotAnyBypass,
                Value::Domain(cond("name", Operator::Eq, "y")),
            ),
        ])
    );
    assert_eq!(
        rule.reads,
        [
            field(SOME_OBJ, "categ_id"),
            field(CATEG, "name"),
            field(SOME_OBJ, "tag_ids"),
        ]
    );
    assert_eq!(subselects(&rule, SOME_OBJ), [model(CATEG).id]);
    let e = err("[('categ_id', 'any!', [])]");
    assert_eq!(
        e.to_string(),
        "ValueError: Domain() invalid item in domain: ('categ_id', 'any!', [])"
    );
    let e = err("[('val', 'any', [])]");
    assert_eq!(
        e.to_string(),
        "The 'any' operator works only on relational fields in condition ('val', 'any', [])"
    );
}

#[test]
fn inherited_fields_read_through_their_delegate() {
    let rule = compile_on(CHILD, "[('val', '>', 0)]").unwrap();
    assert_eq!(
        rule.reads,
        [
            field(CHILD, "val"),
            field(CHILD, "some_id"),
            field(SOME_OBJ, "val"),
        ]
    );
}

#[test]
fn mentions_company_looks_through_the_whole_tree() {
    assert!(ok("[('partner_id', 'any', [('company_id', 'in', company_ids)])]").mentions_company);
    assert!(ok("[('partner_id.company_id.name', '=', 'x')]").mentions_company);
    assert!(ok("['!', ('company_id', '=', 1)]").mentions_company);
    assert!(ok("[('company_id', 'access', 'read')]").mentions_company);
    assert!(ok("[('user_id', 'in', user.company_ids.ids)]").mentions_company);
    assert!(!ok("[('val', '>', 0), ('user_id', '=', user.id)]").mentions_company);
}

/// The whole case is `spec_dom_035_access_operator`
/// (`tests/spec_whole.rs`).
#[test]
fn access_conditions_are_kept_and_checked() {
    let rule = ok("[('categ_id', 'access', 'read'), ('id', 'access', 'write')]");
    assert_eq!(
        rule.domain,
        Domain::and([
            cond("categ_id", Operator::Access, "read"),
            cond("id", Operator::Access, "write"),
        ])
    );
    assert_eq!(
        rule.access,
        [
            AccessLeaf {
                target: model(CATEG).id,
                op: Operation::Read,
            },
            AccessLeaf {
                target: model(SOME_OBJ).id,
                op: Operation::Write,
            },
        ]
    );
    for (text, message) in [
        (
            "[('child_ids', 'access', 'read')]",
            "The 'access' operator works only for many2one and 'id' fields in condition \
             ('child_ids', 'access', 'read')",
        ),
        (
            "[('val', 'access', 'read')]",
            "The 'access' operator works only for many2one and 'id' fields in condition ('val', \
             'access', 'read')",
        ),
        (
            "[('categ_id', 'access', 'blabla')]",
            "Invalid value for 'access' operator in condition ('categ_id', 'access', 'blabla')",
        ),
        (
            "[('categ_id.name', 'access', 'read')]",
            "The 'access' operator does not work for properties in condition ('categ_id.name', \
             'access', 'read')",
        ),
    ] {
        assert_eq!(err(text).to_string(), message, "{text}");
    }
}

#[test]
fn malformed_domains_use_references_texts() {
    for (text, message) in [
        (
            "[(1, '!=', 1)]",
            "TypeError: Domain() invalid arguments: (1, '!=', 1)".to_owned(),
        ),
        (
            "[('non_existing_field', '=', 'value')]",
            "Invalid field test_access_right.some_obj.non_existing_field in condition \
             ('non_existing_field', '=', 'value')"
                .to_owned(),
        ),
        (
            "[('val', 'like like', 1)]",
            "Invalid operator in condition ('val', 'like like', 1)".to_owned(),
        ),
        (
            "[('', '=', 1)]",
            "Empty field name in condition ('', '=', 1)".to_owned(),
        ),
        (
            "['|', ('val', '=', 1)]",
            "ValueError: Domain() malformed domain ['|', ('val', '=', 1)]".to_owned(),
        ),
        (
            "[('val', '=', 1), 'x']",
            "ValueError: Domain() invalid item in domain: 'x'".to_owned(),
        ),
        (
            "'A really bad domain!'",
            "TypeError: Domain() invalid argument type for domain: 'A really bad domain!'"
                .to_owned(),
        ),
    ] {
        let e = err(text);
        assert!(matches!(e, RuleError::Invalid { .. }), "{text}: {e:?}");
        assert_eq!(e.to_string(), message, "{text}");
    }
    let text = "A really bad domain!";
    let e = err(text);
    let parse = erp_expr::parse(text).unwrap_err();
    assert_eq!(e, RuleError::Parse(parse.clone()));
    assert_eq!(e.to_string(), parse.to_string());
}

#[test]
fn spec_sec_016_access_domain_validation() {
    let registry = registry();
    for text in [
        "A really bad domain!",
        "[(1, '!=', 1)]",
        "[('non_existing_field', '=', 'value')]",
    ] {
        let e = validate(registry, SOME_OBJ, Some(text)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation, "{text}");
        let reason = compile_on(SOME_OBJ, text).unwrap_err().to_string();
        assert_eq!(e.message(), format!("Invalid domain {text}: {reason}"));
    }
    for text in [
        None,
        Some("False"),
        Some("[(1, '=', 1)]"),
        Some("[('val', '=', 12)]"),
    ] {
        validate(registry, SOME_OBJ, text).unwrap();
    }
    // a domain on a rule of ir.access itself; no domain is fine
    let e = validate(registry, "ir.access", Some("[('name', '=', 'x')]")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "Accesses with domain can not be applied on model Access itself."
    );
    assert_eq!(
        validate(registry, "ir.access", Some(" "))
            .unwrap_err()
            .message(),
        "Accesses with domain can not be applied on model Access itself."
    );
    validate(registry, "ir.access", None).unwrap();
    validate(registry, "ir.access", Some("")).unwrap();
    let e = validate(registry, "nope.model", Some("[]")).unwrap_err();
    // a row without domain has nothing to validate
    validate(registry, "nope.model", None).unwrap();
    assert_eq!(
        e.message(),
        "Invalid domain []: the model 'nope.model' does not exist"
    );
}

#[test]
fn inputs_are_bounded() {
    // the expression source limit (64 KiB)
    let long = format!("[('name', '=', '{}')]", "x".repeat(70 * 1024));
    let e = err(&long);
    let RuleError::Parse(parse) = &e else {
        panic!("expected a parse error, got {e:?}");
    };
    assert_eq!(parse.class(), "LimitExceeded");
    // the parser's nesting limit
    let deep = format!("{}{}", "[".repeat(150), "]".repeat(150));
    let RuleError::Parse(parse) = err(&deep) else {
        panic!("expected a parse error");
    };
    assert_eq!(parse.class(), "LimitExceeded");
    // operators nested deeper than erp_domain::MAX_DEPTH
    let nots = format!("[{} ('val', '=', 1)]", "'!', ".repeat(300));
    assert_eq!(
        err(&nots).to_string(),
        "the domain nests more than 256 operators"
    );
    // long flat chains of one connective do not nest
    let alternatives = (0..300)
        .map(|i| format!("('val', '=', {i})"))
        .collect::<Vec<_>>();
    let ors = format!("[{} {}]", "'|', ".repeat(299), alternatives.join(", "));
    assert!(matches!(ok(&ors).domain.kind(), erp_domain::DomainKind::Or(c) if c.len() == 300));
    // the item bound, through the builder (a 64 KiB source cannot reach it)
    let operators = crate::rule_op::Operators::none();
    let mut builder = Builder::new(registry(), &operators);
    let condition = Sym::Seq {
        items: vec![
            Sym::Lit(Value::Str("val".to_owned())),
            Sym::Lit(Value::Str("=".to_owned())),
            Sym::Lit(Value::Int(1)),
        ],
        tuple: true,
    };
    let items: Vec<Sym> = vec![condition; MAX_RULE_ITEMS + 1];
    let e = builder.domain(model(SOME_OBJ), &items).unwrap_err();
    assert_eq!(e.to_string(), "the domain has more than 10000 items");
}

#[test]
fn rules_on_ir_access_are_refused() {
    let access = model("ir.access");
    assert_eq!(
        compile(registry(), access, Some("[('name', '=', 'x')]")).unwrap_err(),
        RuleError::OnAccessModel
    );
    assert!(compile(registry(), access, None).unwrap().domain.is_true());
}
