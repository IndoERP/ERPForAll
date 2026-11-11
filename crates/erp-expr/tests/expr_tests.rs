//! Evaluator behaviour beyond the CPython goldens: host context, scoping, host-provided names,
//! the data-loader context, the modifier combiner, syntax
//! errors, and deliberate divergences from Python.

// Test helpers outside `#[test]` functions fail loudly too (clippy.toml only covers the tests).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use erp_expr::{
    Budget, EvalContext, EvalLimits, ExprError, ExprObject, ExprValue, HostHooks, Kwargs,
    combine_python_attribute, eval,
};

fn loader_ctx() -> EvalContext {
    EvalContext::standard()
        .with_clock(fixed_now)
        .with_ref_resolver(|xml_id, raise| match xml_id {
            "base.group_user" => Ok(ExprValue::Int(4)),
            "base.main_company" => Ok(ExprValue::Int(1)),
            _ if !raise => Ok(ExprValue::Bool(false)),
            _ => Err(ExprError::value_error(format!(
                "External ID not found in the system: {xml_id}"
            ))),
        })
}

#[test]
fn spec_data_012_eval_values_of_data_files() {
    let ctx = loader_ctx();
    assert_eq!(
        ok_in(&ctx, "[Command.set([ref('base.group_user')])]").repr(),
        "[(6, 0, [4])]"
    );
    assert_eq!(
        ok_in(&ctx, "[(4, ref('base.group_user'))]").repr(),
        "[(4, 4)]"
    );
    assert_eq!(
        ok_in(
            &ctx,
            "[Command.link(ref('base.group_user')), Command.clear()]"
        )
        .repr(),
        "[(4, 4, 0), (5, 0, 0)]"
    );
    assert_eq!(
        ok_in(
            &ctx,
            "Command.create({'name': 'Vendor', 'company_id': ref('base.main_company')})"
        )
        .repr(),
        "(0, 0, {'name': 'Vendor', 'company_id': 1})"
    );
    assert_eq!(
        ok_in(&ctx, "ref('base.missing', raise_if_not_found=False)"),
        ExprValue::Bool(false)
    );
    assert_eq!(
        ok_in(&ctx, "ref('base.missing', False)"),
        ExprValue::Bool(false)
    );
    assert_err_in(
        &ctx,
        "ref('base.missing')",
        "ValueError",
        "ValueError: External ID not found in the system: base.missing",
    );
    assert_err_in(
        &ctx,
        "ref('')",
        "ValueError",
        "ValueError: ref() needs a non-empty XML id",
    );
    assert_err_in(
        &ctx,
        "ref(xml='x')",
        "TypeError",
        "TypeError: ref() got an unexpected keyword argument 'xml'",
    );
    assert_eq!(
        ok_in(&ctx, "'res.partner,%d' % ref('base.main_company')"),
        ExprValue::str("res.partner,1")
    );
    // search="[('code', '=', 'BE')]"
    assert_eq!(
        ok_in(&ctx, "[('code', '=', 'BE')]").repr(),
        "[('code', '=', 'BE')]"
    );
    // Date expressions of demo data use the host clock.
    assert_eq!(
        ok_in(
            &ctx,
            "(DateTime.today() - relativedelta(days=3)).strftime('%Y-%m-%d %H:%M')"
        ),
        ExprValue::str("2026-10-12 12:34")
    );
    assert_eq!(
        ok_in(&ctx, "time.strftime('%Y-%m')+'-15'"),
        ExprValue::str("2026-10-15")
    );
    assert_eq!(
        ok_in(&ctx, "datetime.now() + timedelta(days=1, hours=2)").repr(),
        "datetime.datetime(2026, 10, 16, 14, 34, 56)"
    );
    assert_eq!(
        ok_in(
            &ctx,
            "(datetime.today().date() + relativedelta(months=1, day=17, weekday=0))"
        )
        .repr(),
        "datetime.date(2026, 11, 23)"
    );
    assert_eq!(
        ok_in(&ctx, "time.time()"),
        ExprValue::Float(1_792_067_696.0)
    );
}

#[test]
fn date_functions_need_a_host_clock() {
    assert_err(
        "datetime.now()",
        "ValueError",
        "ValueError: datetime.now() needs a clock: the host must provide one with EvalContext::with_clock",
    );
    assert_err(
        "time.strftime('%Y')",
        "ValueError",
        "ValueError: time.strftime() needs a clock: the host must provide one with EvalContext::with_clock",
    );
    assert_err(
        "datetime.now(pytz.utc)",
        "NameError",
        "NameError: name 'pytz' is not defined",
    );
    assert_err_in(
        &loader_ctx(),
        "datetime.now('UTC')",
        "ValueError",
        "ValueError: timezone-aware datetimes are not supported in expressions (naive UTC only)",
    );
    assert_err(
        "datetime.combine",
        "AttributeError",
        "AttributeError: type object 'datetime.datetime' has no attribute 'combine'",
    );
    assert_err(
        "time.gmtime()",
        "AttributeError",
        "AttributeError: module 'time' has no attribute 'gmtime'",
    );
}

#[test]
fn host_provided_names_are_never_invented() {
    let obj_msg = "record access is provided by the data loader (EvalContext::with_model)";
    let env_msg = "environment access is provided via obj().env (EvalContext::with_model)";
    let m3 = "user and group checks are provided by the host and are not available before M3";
    assert_err(
        "obj().env.ref('base.user_admin').id",
        "NameError",
        &format!("NameError: name 'obj' is not available in this context: {obj_msg}"),
    );
    assert_err(
        "env.user.has_group('base.group_user')",
        "NameError",
        &format!("NameError: name 'env' is not available in this context: {env_msg}"),
    );
    assert_err(
        "user.has_group('base.group_user')",
        "NameError",
        &format!("NameError: name 'user' is not available in this context: {m3}"),
    );
    assert_err(
        "has_group('base.group_user')",
        "NameError",
        &format!("NameError: name 'has_group' is not available in this context: {m3}"),
    );
    assert_err(
        "ref('base.group_user')",
        "NameError",
        "NameError: name 'ref' is not available in this context: XML-id references are provided by the data loader (EvalContext::with_ref_resolver)",
    );
    assert_err(
        "version",
        "NameError",
        "NameError: name 'version' is not available in this context: the the reference ERP series is provided by the host (EvalContext::set)",
    );
    // Once the host provides a name, it is used as given.
    let ctx = EvalContext::standard().with_var("version", ExprValue::str("20.0"));
    assert_eq!(ok_in(&ctx, "version"), ExprValue::str("20.0"));
}

/// A host record exposing only `id`.
#[derive(Debug)]
struct Partner(i64);

impl ExprObject for Partner {
    fn type_name(&self) -> &str {
        "res.partner"
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> erp_expr::ExprResult<ExprValue> {
        match name {
            "id" => Ok(ExprValue::Int(self.0)),
            "huge" => Ok(big_string(2_000_000)),
            _ => Err(ExprError::attribute_error(self.type_name(), name)),
        }
    }
}

#[test]
fn host_objects_fail_loudly_and_their_results_are_charged() {
    let ctx = EvalContext::standard()
        .with_var("partner", ExprValue::Object(Arc::new(Partner(7))))
        .with_var("other", ExprValue::Object(Arc::new(Partner(8))));
    assert_eq!(ok_in(&ctx, "partner.id + 1"), ExprValue::Int(8));
    assert_err_in(
        &ctx,
        "partner.name",
        "AttributeError",
        "AttributeError: 'res.partner' object has no attribute 'name'",
    );
    assert_err_in(
        &ctx,
        "partner | other",
        "TypeError",
        "TypeError: unsupported operand type(s) for |: 'res.partner' and 'res.partner'",
    );
    assert_err_in(
        &ctx,
        "partner()",
        "TypeError",
        "TypeError: 'res.partner' object is not callable",
    );
    assert_err_in(
        &ctx,
        "partner[0]",
        "TypeError",
        "TypeError: 'res.partner' object is not subscriptable",
    );
    assert_err_in(
        &ctx,
        "partner.huge",
        "LimitExceeded",
        "LimitExceeded: value of 2000032 bytes exceeds the per-value limit of 1048576 bytes",
    );
    assert_eq!(ok_in(&ctx, "partner == partner"), ExprValue::Bool(true));
    assert_eq!(ok_in(&ctx, "partner == other"), ExprValue::Bool(false));
}

#[test]
fn host_functions_receive_the_budget() {
    let mut ctx = EvalContext::standard();
    ctx.set_function(
        "make",
        |args: &[ExprValue], _kw: &Kwargs, budget: &mut Budget| {
            let n = args.first().and_then(ExprValue::as_int).unwrap_or(0);
            let n = usize::try_from(n).map_err(|_| ExprError::value_error("negative"))?;
            budget.alloc(n)?;
            Ok(big_string(n))
        },
    );
    assert_eq!(ok_in(&ctx, "len(make(10))"), ExprValue::Int(10));
    assert_err_in(
        &ctx,
        "make(5000000)",
        "LimitExceeded",
        "LimitExceeded: value of 5000000 bytes exceeds the per-value limit of 1048576 bytes",
    );
    // A host function that does not charge is charged by the evaluator.
    ctx.set_function("lazy", |_: &[ExprValue], _: &Kwargs, _: &mut Budget| {
        Ok(big_string(3_000_000))
    });
    assert_err_in(
        &ctx,
        "lazy()",
        "LimitExceeded",
        "LimitExceeded: value of 3000032 bytes exceeds the per-value limit of 1048576 bytes",
    );
}

#[test]
fn walrus_and_comprehensions_never_write_the_host_context() {
    let ctx = EvalContext::standard().with_var("x", ExprValue::Int(5));
    assert_eq!(ok_in(&ctx, "(x := 10) + x"), ExprValue::Int(20));
    assert_eq!(ctx.get("x"), Some(&ExprValue::Int(5)));
    assert_eq!(ok_in(&ctx, "x"), ExprValue::Int(5));
    assert_eq!(ok_in(&ctx, "[x for x in [1, 2]] + [x]").repr(), "[1, 2, 5]");
    assert_eq!(
        ok_in(&ctx, "([y := x * 2 for x in [1, 2]], y)").repr(),
        "([2, 4], 4)"
    );
    assert!(ctx.get("y").is_none());
    assert_err_in(
        &ctx,
        "[x := 1 for x in [1]]",
        "SyntaxError",
        "SyntaxError: assignment expression cannot rebind comprehension iteration variable 'x'",
    );
    let bare = err_in(&ctx, "x := 1");
    assert_eq!(
        bare.to_string(),
        "SyntaxError: assignment expression cannot be used at top level without parentheses (line 1, column 3)"
    );
    assert!(matches!(bare, ExprError::Parse { offset: 2, .. }));
}

#[test]
fn contexts_are_shared_across_threads() {
    let ctx = Arc::new(EvalContext::standard().with_var("n", ExprValue::Int(21)));
    std::thread::scope(|s| {
        for i in 0..4 {
            let ctx = Arc::clone(&ctx);
            s.spawn(move || {
                for _ in 0..50 {
                    let v = eval(&format!("(k := n * 2) + {i} - k"), &ctx).expect("evaluates");
                    assert_eq!(v, ExprValue::Int(i));
                }
            });
        }
    });
}

#[test]
fn chained_comparison_evaluates_the_middle_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let mut ctx = EvalContext::standard();
    ctx.set_function("mid", move |_: &[ExprValue], _: &Kwargs, _: &mut Budget| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(ExprValue::Int(5))
    });
    assert_eq!(ok_in(&ctx, "1 < mid() < 10"), ExprValue::Bool(true));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(ok_in(&ctx, "10 < mid() < 1/0"), ExprValue::Bool(false));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn identity_compares_only_against_singletons() {
    assert_eq!(ok("None is None"), ExprValue::Bool(true));
    assert_eq!(ok("1 is None"), ExprValue::Bool(false));
    assert_eq!(ok("[] is not None"), ExprValue::Bool(true));
    assert_eq!(ok("1 is True"), ExprValue::Bool(false));
    for src in ["[] is []", "1 is 1", "'a' is not 'a'"] {
        assert_err(
            src,
            "TypeError",
            "TypeError: 'is' / 'is not' compare only against None, True or False",
        );
    }
}

#[test]
fn call_spreading_and_keyword_errors() {
    let mut ctx = EvalContext::standard();
    ctx.set_function(
        "count",
        |args: &[ExprValue], kw: &Kwargs, _: &mut Budget| {
            Ok(ExprValue::Int(
                i64::try_from(args.len() + kw.len()).unwrap_or(i64::MAX),
            ))
        },
    );
    assert_eq!(ok_in(&ctx, "count(*[1, 2], 3, *'ab')"), ExprValue::Int(5));
    assert_eq!(ok_in(&ctx, "count(**{'a': 1}, b=2)"), ExprValue::Int(2));
    assert_err_in(
        &ctx,
        "count(*1)",
        "TypeError",
        "TypeError: argument after * must be an iterable",
    );
    assert_err_in(
        &ctx,
        "count(**[1])",
        "TypeError",
        "TypeError: argument after ** must be a mapping, not list",
    );
    assert_err_in(
        &ctx,
        "count(a=1, **{'a': 2})",
        "TypeError",
        "TypeError: got multiple values for keyword argument 'a'",
    );
    assert_err_in(
        &ctx,
        "count(a=1, a=2)",
        "SyntaxError",
        "SyntaxError: keyword argument repeated: a",
    );
    assert_err_in(
        &ctx,
        "count(**{1: 2})",
        "TypeError",
        "TypeError: keywords must be strings",
    );
    assert_err_in(
        &ctx,
        "count(a=1, 2)",
        "SyntaxError",
        "SyntaxError: positional argument follows keyword argument (line 1, column 12)",
    );
}

#[test]
fn syntax_outside_the_subset_names_the_construct() {
    for (src, message) in [
        (
            "lambda x: x",
            "SyntaxError: 'lambda' is not supported in the expression subset (line 1, column 1)",
        ),
        (
            "{1, 2}",
            "SyntaxError: set literals are not supported; use a list or a tuple (line 1, column 3)",
        ),
        (
            "{k: 1 for k in 'a'}",
            "SyntaxError: dict comprehensions are not supported (line 1, column 7)",
        ),
        (
            "{x for x in 'a'}",
            "SyntaxError: set comprehensions are not supported (line 1, column 4)",
        ),
        (
            "{**d}",
            "SyntaxError: '**' unpacking in dict displays is not supported (line 1, column 2)",
        ),
        (
            "x[1, 2]",
            "SyntaxError: tuple subscripts are not supported (line 1, column 4)",
        ),
        (
            "",
            "SyntaxError: unexpected end of expression in expression (line 1, column 1)",
        ),
        (
            "1 +",
            "SyntaxError: unexpected end of expression in expression (line 1, column 4)",
        ),
        (
            "(1",
            "SyntaxError: expected ')', found end of expression (line 1, column 3)",
        ),
        (
            "1 2",
            "SyntaxError: unexpected integer 2 after expression (line 1, column 3)",
        ),
        (
            "café",
            "SyntaxError: invalid character 'é' (U+00E9) (line 1, column 4)",
        ),
        (
            "012",
            "SyntaxError: leading zeros in decimal integer literals are not permitted (line 1, column 1)",
        ),
        (
            "99999999999999999999",
            "SyntaxError: integer literal does not fit in 64 bits (line 1, column 1)",
        ),
        (
            "'\\N{DASH}'",
            "SyntaxError: \\N{...} named escapes are not supported (line 1, column 1)",
        ),
        (
            "'abc",
            "SyntaxError: Unterminated string literal (line 1, column 1)",
        ),
        (
            "'é' b'x'",
            "SyntaxError: cannot mix bytes and nonbytes literals (line 1, column 5)",
        ),
        (
            "f'{}'",
            "SyntaxError: f-string: empty expression not allowed (line 1, column 1)",
        ),
        (
            "f'{x!z}'",
            "SyntaxError: f-string: invalid conversion character: expected 's', 'r', or 'a' (line 1, column 1)",
        ),
        (
            "f'{x:{w}}'",
            "SyntaxError: f-string: nested replacement fields in format specs are not supported (line 1, column 1)",
        ),
        (
            "f'}'",
            "SyntaxError: f-string: single '}' is not allowed (line 1, column 1)",
        ),
    ] {
        let e = err(src);
        assert_eq!(e.class(), "SyntaxError", "{src}");
        assert_eq!(e.to_string(), message, "{src}");
    }
}

#[test]
fn source_text_edge_cases() {
    assert_eq!(ok("1 +\r\n 2"), ExprValue::Int(3));
    assert_eq!(ok("1 + \\\n 2"), ExprValue::Int(3));
    assert_eq!(ok("[1, # comment\n 2]").repr(), "[1, 2]");
    assert_eq!(ok("'é' + 'ü'"), ExprValue::str("éü"));
    assert_eq!(ok("'日本'[1]"), ExprValue::str("本"));
    assert_eq!(ok("len('😀é')"), ExprValue::Int(2));
    assert_eq!(ok("'\\é'"), ExprValue::str("\\é"));
    assert_eq!(ok("r'\\''"), ExprValue::str("\\'"));
    assert_eq!(ok("f'{\"é\":>3}'"), ExprValue::str("  é"));
    assert_eq!(ok("'%-3s|' % 'é'"), ExprValue::str("é  |"));
    assert_eq!(ok("{1: 'a', '1': 'b'}[1]"), ExprValue::str("a"));
}

/// Deliberate divergences from CPython, each with its reason.
#[test]
fn divergences_from_python_are_loud() {
    // 64-bit integers.
    assert_err(
        "2 ** 63 - 1",
        "OverflowError",
        "OverflowError: integer result does not fit in 64 bits",
    );
    // `key=` and `str(bytes, encoding)` are not part of the subset.
    assert_err(
        "min(1, key=None)",
        "TypeError",
        "TypeError: min() got an unexpected keyword argument 'key'",
    );
    assert_err(
        "str(1, 2)",
        "TypeError",
        "TypeError: str() takes at most 1 argument (2 given)",
    );
    assert_err(
        "Command.create(1)",
        "TypeError",
        "TypeError: Command.create() argument 'values' must be dict, not int",
    );
    assert_err(
        "Command.set(1)",
        "TypeError",
        "TypeError: Command.set() argument 'ids' must be a list or tuple of ids, not int",
    );
    assert_err(
        "Command.set(['a'])",
        "TypeError",
        "TypeError: Command.set() id #0 must be int, not str",
    );
    // Generator expressions are evaluated eagerly into a list.
    assert_eq!(repr("(x for x in [1, 2])"), "[1, 2]");
    // dict views are lists.
    assert_eq!(repr("{'a': 1}.items()"), "[('a', 1)]");
}

#[test]
fn spec_data_054_modifier_combiner() {
    let combine = |cur: Option<&str>, add: Option<&str>, rem: Option<&str>, sep: &str| {
        combine_python_attribute(cur, add, rem, Some(sep), "invisible").expect("combines")
    };
    assert_eq!(
        combine(None, Some("name == 'a'"), None, "and").as_deref(),
        Some("name == 'a'")
    );
    assert_eq!(
        combine(Some("name == 'a'"), Some("name == 'b'"), None, "or").as_deref(),
        Some("(name == 'a') or (name == 'b')")
    );
    let r1 = combine(
        Some("name == 'e' and name == 'f'"),
        Some("id == 33"),
        None,
        "and",
    );
    let r2 = combine(r1.as_deref(), Some("id == 42"), None, "or");
    let r3 = combine(r2.as_deref(), Some("id == 1"), None, " and ");
    assert_eq!(
        r3.as_deref(),
        Some("(((name == 'e' and name == 'f') and (id == 33)) or (id == 42)) and (id == 1)")
    );
    assert_eq!(
        combine(
            Some("(((id == 1) and (id == 2)) and (id == 3)) and (id == 4)"),
            None,
            Some("id == 2"),
            "and"
        )
        .as_deref(),
        Some("(((id == 1)) and (id == 3)) and (id == 4)")
    );
    assert_eq!(combine(Some("id == 1"), None, Some("id == 1"), "and"), None);
    assert_eq!(
        combine(Some("1 or not name"), None, Some("1"), "or").as_deref(),
        Some("not name")
    );
    let bad = combine_python_attribute(Some("x"), Some("y"), None, Some("else"), "invisible")
        .expect_err("invalid separator");
    assert_eq!(
        bad.to_string(),
        "ModifierError: Invalid separator 'else' for python expression 'invisible'; valid values are 'and' and 'or'"
    );
    let missing = combine_python_attribute(Some("x"), Some("y"), None, None, "readonly")
        .expect_err("missing separator");
    assert_eq!(
        missing.to_string(),
        "ModifierError: Missing separator for python expression 'readonly'; valid values are 'and' and 'or'"
    );
}

#[test]
fn values_have_python_reprs_and_type_strict_equality() {
    assert!(ExprValue::Int(1).py_eq(&ExprValue::Float(1.0)));
    assert_ne!(ExprValue::Int(1), ExprValue::Float(1.0));
    let d = ExprValue::dict(vec![(ExprValue::Int(1), ExprValue::str("a"))]).expect("hashable");
    assert_eq!(d.repr(), "{1: 'a'}");
    let unhashable = ExprValue::dict(vec![(ExprValue::list(vec![]), ExprValue::None)])
        .expect_err("list keys are unhashable");
    assert_eq!(unhashable.to_string(), "TypeError: unhashable type: 'list'");
    assert_eq!(
        ExprValue::dict_str([("k", ExprValue::Int(1))]).to_string(),
        "{'k': 1}"
    );
    assert_eq!(ExprValue::tuple(vec![ExprValue::Int(1)]).repr(), "(1,)");
    assert_eq!(
        ExprValue::list(vec![ExprValue::str("x")]).deep_size(),
        2 * ExprValue::None.deep_size() + 1
    );
}

#[test]
fn expr_model_proxy_and_browse() {
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_ref_resolver(|xid, _| {
            if xid == "base.partner_admin" {
                Ok(ExprValue::Int(3))
            } else {
                Err(ExprError::value_error("not found"))
            }
        })
        .with_xid_model_resolver(|xid| {
            if xid == "base.partner_admin" {
                Some(("res.partner".to_string(), 3))
            } else {
                None
            }
        });

    assert_eq!(ok_in(&ctx, "bool(obj())"), ExprValue::Bool(false));
    assert_eq!(ok_in(&ctx, "obj().id"), ExprValue::Bool(false));
    assert_eq!(ok_in(&ctx, "obj().ids"), ExprValue::list(vec![]));
    assert_eq!(ok_in(&ctx, "obj(1).id"), ExprValue::Int(1));
    assert_eq!(
        ok_in(&ctx, "obj(1).ids"),
        ExprValue::list(vec![ExprValue::Int(1)])
    );
    assert_eq!(
        ok_in(&ctx, "obj(ref('base.partner_admin')).id"),
        ExprValue::Int(3)
    );
    assert_eq!(
        ok_in(&ctx, "obj().env.ref('base.partner_admin').id"),
        ExprValue::Int(3)
    );
    assert_eq!(ok_in(&ctx, "obj().sudo().browse(5).id"), ExprValue::Int(5));
    assert_eq!(
        ok_in(&ctx, "obj().with_context().browse(5).id"),
        ExprValue::Int(5)
    );
    assert_eq!(
        ok_in(&ctx, "obj().sudo(True).browse(5).id"),
        ExprValue::Int(5)
    );
    assert_eq!(ok_in(&ctx, "obj(False).ids"), ExprValue::list(vec![]));
    assert_eq!(ok_in(&ctx, "bool(obj(None))"), ExprValue::Bool(false));
    assert_eq!(ok_in(&ctx, "obj(obj(4)).id"), ExprValue::Int(4));
    assert_eq!(
        ok_in(&ctx, "obj().env['res.partner'].browse(7).id"),
        ExprValue::Int(7)
    );
}

#[test]
fn expr_record_attribute_access() {
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_record_resolver(|model, id, field| match (model, id, field) {
            ("res.partner", 3, "name") => Ok(ExprValue::str("Administrator")),
            ("res.partner", 3, "email") => Ok(ExprValue::str("admin@example.com")),
            ("res.partner", 3, "company_id") => Ok(ExprValue::Int(1)),
            ("res.partner", 3, "active") => Ok(ExprValue::Bool(true)),
            _ => Err(ExprError::attribute_error(model, field)),
        });

    assert_eq!(ok_in(&ctx, "obj(3).name"), ExprValue::str("Administrator"));
    assert_eq!(
        ok_in(&ctx, "obj(3).name.lower()"),
        ExprValue::str("administrator")
    );
    assert_eq!(
        ok_in(&ctx, "obj(3).name.split(' ')[0]"),
        ExprValue::str("Administrator")
    );
    assert_eq!(
        ok_in(&ctx, "obj(3).email"),
        ExprValue::str("admin@example.com")
    );
    assert_eq!(ok_in(&ctx, "obj(3).company_id"), ExprValue::Int(1));
    assert_eq!(ok_in(&ctx, "obj(3).active"), ExprValue::Bool(true));
}

#[test]
fn expr_model_proxy_search() {
    let ctx = EvalContext::standard()
        .with_model("hr.work.entry.type")
        .with_search_resolver(|query| {
            if query.model == "hr.work.entry.type" {
                if query.limit == Some(1) {
                    Ok(vec![101])
                } else {
                    Ok(vec![101, 102, 103])
                }
            } else {
                Ok(vec![])
            }
        });

    assert_eq!(
        ok_in(&ctx, "obj().search([('code', '=', '016.00')]).ids"),
        ExprValue::list(vec![
            ExprValue::Int(101),
            ExprValue::Int(102),
            ExprValue::Int(103)
        ])
    );
    assert_eq!(
        ok_in(&ctx, "obj().search([('code', '=', '016.00')], limit=1).id"),
        ExprValue::Int(101)
    );
    assert_eq!(
        ok_in(&ctx, "obj().sudo().search([('code', '=', '016.00')]).ids"),
        ExprValue::list(vec![
            ExprValue::Int(101),
            ExprValue::Int(102),
            ExprValue::Int(103)
        ])
    );
}

#[test]
fn expr_fail_loudly_on_invalid_obj_access() {
    let ctx = EvalContext::standard().with_model("res.partner");

    assert_err_in(
        &ctx,
        "obj(1).non_existent_field",
        "AttributeError",
        "AttributeError: 'res.partner' object has no attribute 'non_existent_field'",
    );
    assert_err_in(
        &ctx,
        "obj(1, 2)",
        "TypeError",
        "TypeError: browse() takes at most 1 argument (2 given)",
    );
    assert_err_in(
        &ctx,
        "obj('not_an_id')",
        "TypeError",
        "TypeError: browse() id must be an integer or a record, not 'str'",
    );
}

/// `obj(<object>)` accepts only records; ids are positive (review D3 and its NIT).
#[test]
fn expr_browse_rejects_non_records_and_non_positive_ids() {
    let ctx = EvalContext::standard().with_model("res.partner");
    assert_err_in(
        &ctx,
        "obj(obj().env)",
        "TypeError",
        "TypeError: browse() id must be an integer or a record, not 'Environment'",
    );
    assert_err_in(
        &ctx,
        "obj(obj())",
        "TypeError",
        "TypeError: browse() id must be an integer or a record, not 'res.partner'",
    );
    assert_err_in(
        &ctx,
        "obj(True)",
        "TypeError",
        "TypeError: browse() id must be an integer or a record, not 'bool'",
    );
    assert_err_in(
        &ctx,
        "obj(-5)",
        "ValueError",
        "ValueError: res.partner record ids are positive integers, got -5",
    );
    assert_err_in(
        &ctx,
        "obj(0)",
        "ValueError",
        "ValueError: res.partner record ids are positive integers, got 0",
    );
    assert_err_in(
        &ctx,
        "obj(obj().env['res.users'].browse(3))",
        "TypeError",
        "TypeError: browse() takes a res.partner record, not 'res.users'",
    );
    assert_eq!(
        erp_expr::RecordProxy::new("res.partner", 0)
            .map(|r| r.id())
            .expect_err("id 0 is not a record"),
        ExprError::value_error("res.partner record ids are positive integers, got 0")
    );
    assert_eq!(
        erp_expr::RecordsetProxy::new("res.partner", vec![1, -2])
            .map(|r| r.ids().len())
            .expect_err("-2 is not a record id"),
        ExprError::value_error("res.partner record ids are positive integers, got -2")
    );
}

/// Host data the data loader does not provide is never invented (review S3).
#[test]
fn expr_host_data_is_never_invented() {
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|_| Ok(vec![1]));
    for (src, name, what) in [
        ("obj().env.user", "env.user", "the current user"),
        (
            "obj().env.user.has_group('base.group_user')",
            "env.user",
            "the current user",
        ),
        ("obj().env.uid", "env.uid", "the current user id"),
        ("obj().env.company", "env.company", "the current company"),
        (
            "obj().env.companies",
            "env.companies",
            "the allowed companies",
        ),
        ("obj().env.context", "env.context", "the evaluation context"),
    ] {
        let e = err_in(&ctx, src);
        assert_eq!(
            e,
            ExprError::unavailable(
                name,
                format!("{what} is not configured on this environment (host context)")
            ),
            "`{src}`"
        );
        assert_eq!(e.class(), "NameError", "`{src}`");
    }
    // `has_group` fails loudly when group_check resolver is not configured
    assert_eq!(
        err_in(&ctx, "has_group('base.group_user')"),
        ExprError::unavailable(
            "has_group",
            "user and group checks are provided by the host and are not available before M3"
        )
    );
    // `env.ref` needs the model of the external id; a context with only `ref()` cannot give it
    let only_ref = EvalContext::standard()
        .with_model("res.partner")
        .with_ref_resolver(|_, _| Ok(ExprValue::Int(3)));
    assert_err_in(
        &only_ref,
        "obj().env.ref('base.partner_admin')",
        "NameError",
        "NameError: name 'env.ref' is not available in this context: the model of an external \
         id is provided by the data loader (EvalContext::with_xid_model_resolver)",
    );
    // a host whose `ref()` knows an id the model resolver does not is inconsistent
    let inconsistent = only_ref.with_xid_model_resolver(|_| None);
    assert_err_in(
        &inconsistent,
        "obj().env.ref('base.partner_admin')",
        "ValueError",
        "ValueError: External ID base.partner_admin has a record id but no model",
    );
}

/// `search()` arguments are checked, never dropped (review S3).
#[test]
fn expr_search_arguments_fail_loudly() {
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|query| {
            Ok(vec![1; usize::try_from(query.limit.unwrap_or(2)).unwrap()])
        });
    assert_eq!(
        ok_in(&ctx, "obj().search([], limit=None).ids"),
        ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(1)])
    );
    assert_eq!(
        ok_in(&ctx, "obj().search([], 0, 1).ids"),
        ExprValue::list(vec![ExprValue::Int(1)])
    );
    assert_err_in(
        &ctx,
        "obj().search([], limit=-1)",
        "ValueError",
        "ValueError: search() limit must be a positive integer or None, got -1",
    );
    assert_err_in(
        &ctx,
        "obj().search([], limit=0)",
        "ValueError",
        "ValueError: search() limit must be a positive integer or None, got 0",
    );
    assert_err_in(
        &ctx,
        "obj().search([], limit='1')",
        "TypeError",
        "TypeError: search() limit must be an integer or None, not 'str'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], limit=True)",
        "TypeError",
        "TypeError: search() limit must be an integer or None, not 'bool'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], offset=-1)",
        "ValueError",
        "ValueError: search() offset must be a non-negative integer or None, got -1",
    );
    assert_err_in(
        &ctx,
        "obj().search([], offset='0')",
        "TypeError",
        "TypeError: search() offset must be an integer or None, not 'str'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], offset=True)",
        "TypeError",
        "TypeError: search() offset must be an integer or None, not 'bool'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], order=123)",
        "TypeError",
        "TypeError: search() order must be a string or None, not 'int'",
    );
    assert_err_in(
        &ctx,
        "obj().search()",
        "TypeError",
        "TypeError: search() missing 1 required positional argument: 'domain'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], limitt=1)",
        "TypeError",
        "TypeError: search() got an unexpected keyword argument 'limitt'",
    );
    assert_err_in(
        &ctx,
        "obj().search([], 0, 1, limit=1)",
        "TypeError",
        "TypeError: search() got multiple values for argument 'limit'",
    );
}

/// Host context, security groups, context overrides, and sudo(False) chaining into search.
#[test]
fn expr_host_hooks_context_and_overrides() {
    type RecordedQuery = (String, u64, Option<u64>, Option<String>, bool, ExprValue);
    let last_query: Arc<std::sync::Mutex<Option<RecordedQuery>>> =
        Arc::new(std::sync::Mutex::new(None));
    let last_q = Arc::clone(&last_query);

    let hooks = HostHooks::new()
        .with_user_id(2)
        .with_company_id(1)
        .with_company_ids(vec![1, 3])
        .with_context(
            ExprValue::dict(vec![
                (ExprValue::str("lang"), ExprValue::str("en_US")),
                (
                    ExprValue::str("allowed_company_ids"),
                    ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(3)]),
                ),
            ])
            .unwrap(),
        )
        .with_sudo(true)
        .with_record_resolver(|model, id, field| match (model, id, field) {
            ("res.users", 2, "name") => Ok(ExprValue::str("Marc Demo")),
            ("res.company", 1, "name") => Ok(ExprValue::str("YourCompany")),
            _ => Err(ExprError::attribute_error(model, field)),
        })
        .with_group_checker(|xmlid| match xmlid {
            "base.group_user" => Ok(true),
            "base.group_system" => Ok(false),
            _ => Err(ExprError::value_error(format!("no such group: {xmlid}"))),
        })
        .with_search_resolver(move |q| {
            *last_q.lock().unwrap() = Some((
                q.model.to_string(),
                q.offset,
                q.limit,
                q.order.map(|s| s.to_string()),
                q.sudo,
                q.context.clone(),
            ));
            Ok(vec![10, 20])
        });

    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_host_hooks(Arc::new(hooks));

    // env.user, uid, company, companies, context, su
    assert_eq!(ok_in(&ctx, "obj().env.uid"), ExprValue::Int(2));
    assert_eq!(ok_in(&ctx, "obj().env.user.id"), ExprValue::Int(2));
    assert_eq!(
        ok_in(&ctx, "obj().env.user.name"),
        ExprValue::str("Marc Demo")
    );
    assert_eq!(ok_in(&ctx, "obj().env.company.id"), ExprValue::Int(1));
    assert_eq!(
        ok_in(&ctx, "obj().env.company.name"),
        ExprValue::str("YourCompany")
    );
    assert_eq!(
        ok_in(&ctx, "obj().env.companies.ids"),
        ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(3)])
    );
    assert_eq!(ok_in(&ctx, "obj().env.su"), ExprValue::Bool(true));

    // has_group
    assert_eq!(
        ok_in(&ctx, "obj().env.user.has_group('base.group_user')"),
        ExprValue::Bool(true)
    );
    assert_eq!(
        ok_in(&ctx, "obj().env.user.has_group('base.group_system')"),
        ExprValue::Bool(false)
    );
    assert_eq!(
        ok_in(&ctx, "has_group('base.group_user')"),
        ExprValue::Bool(true)
    );
    assert_eq!(
        ok_in(&ctx, "has_group('base.group_system')"),
        ExprValue::Bool(false)
    );
    // The host checks groups for the current user only: another user's record refuses
    // instead of answering with the current user's membership.
    assert_eq!(
        ok_in(
            &ctx,
            "obj().env['res.users'].browse(2).has_group('base.group_user')"
        ),
        ExprValue::Bool(true)
    );
    let other = err_in(
        &ctx,
        "obj().env['res.users'].browse(7).has_group('base.group_user')",
    );
    assert_eq!(other.class(), "ValueError");
    assert!(
        other
            .to_string()
            .contains("has_group() on res.users(7) is not supported: this host sets no per-user group checker (HostHooks::with_user_group_checker) and answers only for the current user (env.uid=2)"),
        "{other}"
    );

    // Without a per-user checker the refusal above stays; with one, another user's record
    // answers for that user, and an unknown user fails loudly.
    let per_user = EvalContext::standard()
        .with_model("res.partner")
        .with_host_hooks(Arc::new(
            HostHooks::new()
                .with_user_id(2)
                .with_group_checker(|_| Ok(true))
                .with_user_group_checker(|uid, xmlid| match (uid, xmlid) {
                    (7, "base.group_user") => Ok(true),
                    (7, "base.group_system") => Ok(false),
                    _ => Err(ExprError::value_error(format!(
                        "res.users({uid}) cannot be checked for {xmlid}"
                    ))),
                }),
        ));
    assert_eq!(
        ok_in(
            &per_user,
            "obj().env['res.users'].browse(7).has_group('base.group_user')"
        ),
        ExprValue::Bool(true)
    );
    assert_eq!(
        ok_in(
            &per_user,
            "obj().env['res.users'].browse(7).has_group('base.group_system')"
        ),
        ExprValue::Bool(false)
    );
    // the current user keeps the fast path: the per-user checker is not asked
    assert_eq!(
        ok_in(
            &per_user,
            "obj().env['res.users'].browse(2).has_group('base.group_user')"
        ),
        ExprValue::Bool(true)
    );
    let unknown = err_in(
        &per_user,
        "obj().env['res.users'].browse(99).has_group('base.group_user')",
    );
    assert!(
        unknown
            .to_string()
            .contains("res.users(99) cannot be checked for base.group_user"),
        "{unknown}"
    );

    // with_context and sudo chaining into search
    let ids = ok_in(
        &ctx,
        "obj().with_context(active_test=False, tz='UTC').sudo(False).search([('is_company', '=', True)], offset=5, limit=2, order='id desc').ids",
    );
    assert_eq!(
        ids,
        ExprValue::list(vec![ExprValue::Int(10), ExprValue::Int(20)])
    );

    let (model, offset, limit, order, sudo, context) = last_query
        .lock()
        .unwrap()
        .take()
        .expect("search was called");
    assert_eq!(model, "res.partner");
    assert_eq!(offset, 5);
    assert_eq!(limit, Some(2));
    assert_eq!(order.as_deref(), Some("id desc"));
    assert!(!sudo);

    // Context preserved allowed_company_ids and updated keys
    let ExprValue::Dict(ctx_map) = context else {
        panic!("context must be a dict");
    };
    assert_eq!(
        ctx_map.get_str("active_test"),
        Some(&ExprValue::Bool(false))
    );
    assert_eq!(ctx_map.get_str("tz"), Some(&ExprValue::str("UTC")));
    assert_eq!(ctx_map.get_str("lang"), Some(&ExprValue::str("en_US")));
    assert_eq!(
        ctx_map.get_str("allowed_company_ids"),
        Some(&ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(3)]))
    );

    // with_context replacing dict while preserving allowed_company_ids
    ok_in(&ctx, "obj().with_context({'bin_size': True}).search([])");
    let (_, _, _, _, _, context2) = last_query
        .lock()
        .unwrap()
        .take()
        .expect("search was called");
    let ExprValue::Dict(ctx_map2) = context2 else {
        panic!("context must be a dict");
    };
    assert_eq!(ctx_map2.get_str("bin_size"), Some(&ExprValue::Bool(true)));
    assert_eq!(ctx_map2.get_str("lang"), None);
    assert_eq!(
        ctx_map2.get_str("allowed_company_ids"),
        Some(&ExprValue::list(vec![ExprValue::Int(1), ExprValue::Int(3)]))
    );

    // Unset company evaluates to an empty recordset
    let hooks_unset = HostHooks::new().with_user_id(1);
    let ctx_unset = EvalContext::standard()
        .with_model("res.partner")
        .with_host_hooks(Arc::new(hooks_unset));
    assert_eq!(
        ok_in(&ctx_unset, "obj().env.company.id"),
        ExprValue::Bool(false)
    );
    assert_eq!(
        ok_in(&ctx_unset, "obj().env.companies.ids"),
        ExprValue::list(vec![])
    );
}

/// Search results are capped and charged to the evaluation budget (review S1).
#[test]
fn expr_search_results_are_capped_and_charged() {
    let cap = erp_expr::MAX_SEARCH_RESULTS;
    let over = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(move |_| Ok((1..=i64::try_from(cap + 1).unwrap()).collect()));
    assert_err_in(
        &over,
        "obj().search([])",
        "LimitExceeded",
        &format!(
            "LimitExceeded: search() on res.partner gave more than {cap} records, the limit of a \
             recordset in a data expression"
        ),
    );
    // a recordset the host returns costs one slot per id: 1000 ids do not fit 16 KiB
    let total = 16 * 1024;
    let small = EvalContext::standard()
        .with_model("res.partner")
        .with_limits(EvalLimits::default().with_max_total_bytes(total))
        .with_search_resolver(|_| Ok((1..=1000).collect()));
    assert_err_in(
        &small,
        "obj().search([])",
        "LimitExceeded",
        &format!("LimitExceeded: evaluation memory budget of {total} bytes exceeded"),
    );
    // `mapped()` reads one field per record and its list is charged the same way
    let mapped = EvalContext::standard()
        .with_model("res.partner")
        .with_limits(EvalLimits::default().with_max_total_bytes(total))
        .with_search_resolver(|_| Ok((1..=300).collect()))
        .with_record_resolver(|_, _, _| Ok(ExprValue::Int(7)));
    assert_err_in(
        &mapped,
        "obj().search([], limit=300).mapped('x')",
        "LimitExceeded",
        &format!("LimitExceeded: evaluation memory budget of {total} bytes exceeded"),
    );
}

/// A bound method taken from a recordset (`mapped`, `sudo`, `with_context`) keeps its own copy
/// of the ids while the evaluator charges a callable as one slot: the copy is charged to the
/// budget when the method is taken, before it is made (review EXPR-CLOSE-2).
#[test]
fn expr_bound_methods_charge_their_receiver_copy() {
    let rs = erp_expr::RecordsetProxy::new("res.partner", (1..=300).collect()).unwrap();
    let receiver = rs.meta().bytes;
    for name in ["mapped", "sudo", "with_context"] {
        let mut budget = Budget::new(EvalLimits::default());
        let method = rs.get_attr(name, &mut budget).unwrap();
        assert!(
            matches!(method, ExprValue::Callable(_)),
            "{name}: {method:?}"
        );
        assert_eq!(
            budget.bytes_used(),
            receiver,
            "{name}: receiver copy not charged"
        );
    }
    // retained methods add up: the third copy does not fit a budget of two
    let mut budget = Budget::new(EvalLimits::default().with_max_total_bytes(2 * receiver));
    let kept = [
        rs.get_attr("mapped", &mut budget).unwrap(),
        rs.get_attr("sudo", &mut budget).unwrap(),
    ];
    let e = rs.get_attr("with_context", &mut budget).unwrap_err();
    assert_eq!(e.class(), "LimitExceeded", "{e}");
    assert_eq!(
        e.to_string(),
        format!(
            "LimitExceeded: evaluation memory budget of {} bytes exceeded",
            2 * receiver
        )
    );
    assert_eq!(kept.len(), 2);
    // a record's bound methods keep one record
    let one = erp_expr::RecordsetProxy::new("res.partner", vec![7]).unwrap();
    let record = one.get_item(&ExprValue::Int(0), &mut Budget::new(EvalLimits::default()));
    let ExprValue::Object(record) = record.unwrap() else {
        panic!("a recordset item is a record");
    };
    let mut budget = Budget::new(EvalLimits::default());
    record.get_attr("sudo", &mut budget).unwrap();
    assert!(
        budget.bytes_used() > 0,
        "a record's bound method is not charged"
    );
}

/// Methods retained in an evaluation are charged one receiver copy each: taking `mapped` four
/// times from a 300-record recordset costs four copies more than reading `id` four times, the same
/// expression otherwise (review EXPR-CLOSE-2).
#[test]
fn expr_retained_bound_methods_count_against_the_budget() {
    let methods_src = "[r.mapped for r in [obj().search([], limit=300)] * 4]";
    let ids_src = "[r.id for r in [obj().search([], limit=300)] * 4]";
    let with_total = |total: usize| {
        EvalContext::standard()
            .with_model("res.partner")
            .with_limits(EvalLimits::default().with_max_total_bytes(total))
            .with_search_resolver(|_| Ok((1..=300).collect()))
    };
    let ExprValue::List(methods) = ok_in(&with_total(usize::MAX), methods_src) else {
        panic!("a list comprehension gives a list");
    };
    assert_eq!(methods.len(), 4);
    for m in methods.iter() {
        assert!(matches!(m, ExprValue::Callable(_)), "{m:?}");
    }
    // the smallest budget (to 256 bytes) the `id` reads fit
    let step = 256;
    let ids_fit = (1..=4096)
        .map(|k| k * step)
        .find(|&total| eval(ids_src, &with_total(total)).is_ok())
        .expect("the id reads fit 1 MiB");
    let copy = erp_expr::RecordsetProxy::new("res.partner", (1..=300).collect())
        .unwrap()
        .meta()
        .bytes;
    // two copies more than the `id` reads: four retained methods do not fit
    let total = ids_fit + 2 * copy;
    assert_err_in(
        &with_total(total),
        methods_src,
        "LimitExceeded",
        &format!("LimitExceeded: evaluation memory budget of {total} bytes exceeded"),
    );
    // four copies more fit them
    ok_in(&with_total(ids_fit + 4 * copy), methods_src);
}

/// `mapped()` charges each value it reads before reading the next: a budget a few values fill
/// stops the loop there, not after every record was read (review CF-1 of the EXPR-CLOSE fixes).
#[test]
fn expr_mapped_charges_each_value_before_the_next_read() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let total = 64 * 1024;
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_limits(EvalLimits::default().with_max_total_bytes(total))
        .with_search_resolver(|_| Ok((1..=50).collect()))
        .with_record_resolver(move |_, _, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(ExprValue::Object(Arc::new(
                erp_expr::RecordsetProxy::new("res.partner.category", (1..=300).collect()).unwrap(),
            )))
        });
    assert_err_in(
        &ctx,
        "obj().search([], limit=50).mapped('category_id')",
        "LimitExceeded",
        &format!("LimitExceeded: evaluation memory budget of {total} bytes exceeded"),
    );
    let done = reads.load(Ordering::SeqCst);
    assert!(
        done < 10,
        "{done} of 50 values were read before the budget stopped mapped()"
    );
}

/// A record converts to its id only through `int()` (Python's `__int__`); arithmetic, bitwise
/// operators, ordering and indexing never treat it as an int.
#[test]
fn expr_records_are_not_ints_outside_int() {
    let ctx = EvalContext::standard().with_model("res.partner");
    assert_eq!(ok_in(&ctx, "int(obj(7))"), ExprValue::Int(7));
    assert_err_in(
        &ctx,
        "obj(1) + 1",
        "TypeError",
        "TypeError: unsupported operand type(s) for +: 'res.partner' and 'int'",
    );
    assert_err_in(
        &ctx,
        "obj(1) + obj(2)",
        "TypeError",
        "TypeError: unsupported operand type(s) for +: 'res.partner' and 'res.partner'",
    );
    assert_err_in(
        &ctx,
        "obj(1) & obj(1)",
        "TypeError",
        "TypeError: unsupported operand type(s) for &: 'res.partner' and 'res.partner'",
    );
    assert_err_in(
        &ctx,
        "obj(1) < 3",
        "TypeError",
        "TypeError: '<' not supported between instances of 'res.partner' and 'int'",
    );
    assert_err_in(
        &ctx,
        "[10, 20][obj(1)]",
        "TypeError",
        "TypeError: list indices must be integers or slices, not res.partner",
    );
}

/// `sudo()` accepts only `flag` as a keyword.
#[test]
fn expr_sudo_rejects_unknown_keywords() {
    let ctx = EvalContext::standard().with_model("res.partner");
    assert_err_in(
        &ctx,
        "obj().sudo(foo=1)",
        "TypeError",
        "TypeError: sudo() got an unexpected keyword argument 'foo'",
    );
}

#[test]
fn expr_recordset_equality() {
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|q| match q.model {
            "res.partner" => Ok(vec![3, 1, 2]),
            _ => Ok(vec![1]),
        });
    let t = ExprValue::Bool(true);
    let f = ExprValue::Bool(false);
    // the same ids in any order, repeats ignored
    assert_eq!(
        ok_in(
            &ctx,
            "obj().search([]) == obj().browse(1).env['res.partner'].search([])"
        ),
        t
    );
    assert_eq!(ok_in(&ctx, "obj().search([]) == obj().search([])[::-1]"), t);
    assert_eq!(ok_in(&ctx, "obj().search([]) != obj().search([])[:2]"), t);
    assert_eq!(
        ok_in(&ctx, "obj().search([])[:2] == obj().search([])[1:]"),
        f
    );
    // obj() is the empty recordset of its model
    assert_eq!(ok_in(&ctx, "obj() == obj()"), t);
    assert_eq!(ok_in(&ctx, "obj() == obj(False)"), t);
    assert_eq!(ok_in(&ctx, "obj() == obj(1)"), f);
    assert_eq!(ok_in(&ctx, "obj(1) == obj()"), f);
    assert_eq!(ok_in(&ctx, "obj() != obj(1)"), t);
    // a singleton record equals the one-id recordset in both directions
    assert_eq!(ok_in(&ctx, "obj(1) == obj().search([])[1:2]"), t);
    assert_eq!(ok_in(&ctx, "obj(1) == obj().search([])[1]"), t);
    assert_eq!(ok_in(&ctx, "obj(1) == obj().search([])[0:1]"), f);
    assert_eq!(ok_in(&ctx, "obj().search([])[1:2] == obj(1)"), t);
    assert_eq!(ok_in(&ctx, "obj(1) == obj(1)"), t);
    assert_eq!(ok_in(&ctx, "obj(1) == obj(2)"), f);
    // never equal to a non-record, never an error
    assert_eq!(ok_in(&ctx, "obj().search([]) == 3"), f);
    assert_eq!(ok_in(&ctx, "obj() == False"), f);
    assert_eq!(ok_in(&ctx, "obj(1) == 1"), f);
    assert_eq!(ok_in(&ctx, "obj().search([]) != 3"), t);
    // records of different models are not equal, even with the same ids
    assert_eq!(ok_in(&ctx, "obj(1) == obj().env['res.users'].browse(1)"), f);
    assert_eq!(ok_in(&ctx, "obj() == obj().env['res.users']"), f);
    assert_eq!(ok_in(&ctx, "obj() == obj().env['res.partner']"), t);
}

#[test]
fn expr_mapped_reads_every_record_before_failing() {
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let ctx = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|_| Ok((1..=50).collect()))
        .with_record_resolver(move |_, id, field| {
            seen.lock().unwrap().push((id, field.to_owned()));
            Err(ExprError::value_error(format!(
                "{field} of {id} not resolved"
            )))
        });
    // the first error is the one reported, after every record was asked for
    let e = err_in(&ctx, "obj().search([]).mapped('name')");
    assert_eq!(e.to_string(), "ValueError: name of 1 not resolved");
    let ids: Vec<i64> = asked.lock().unwrap().iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, (1..=50).collect::<Vec<_>>());

    // a field the resolver knows for every record still maps in order
    let ok = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|_| Ok(vec![4, 5]))
        .with_record_resolver(|_, id, _| Ok(ExprValue::Int(id * 10)));
    assert_eq!(
        ok_in(&ok, "obj().search([]).mapped('x')").repr(),
        "[40, 50]"
    );
}

#[test]
fn expr_percent_d_on_records() {
    let ctx = EvalContext::standard().with_model("res.partner");
    assert_eq!(ok_in(&ctx, "'%d' % obj(7)"), ExprValue::str("7"));
    assert_eq!(
        ok_in(&ctx, "'%i-%u' % (obj(7), obj(8))"),
        ExprValue::str("7-8")
    );
    assert_eq!(ok_in(&ctx, "'%05d' % obj(7)"), ExprValue::str("00007"));
    for conv in ["x", "o", "c"] {
        let e = err_in(&ctx, &format!("'%{conv}' % obj(7)"));
        assert_eq!(e.class(), "TypeError", "%{conv}: {e}");
    }
    // a recordset of several records has no single id
    let many = EvalContext::standard()
        .with_model("res.partner")
        .with_search_resolver(|_| Ok(vec![1, 2]));
    assert_err_in(
        &many,
        "'%d' % obj().search([])",
        "ValueError",
        "ValueError: Expected singleton: res.partner(2)",
    );
}
