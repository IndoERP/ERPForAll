//! Integration tests for the Luau sandbox. Attack cases follow the review
//! (blocking items 1-8).

// The assertion helpers below are test code; clippy's test exemption covers only `#[test]` bodies.
#![allow(clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use erp_script::{
    HostError, LimitKind, LuauSandbox, PATTERN_MAX_BYTES, PATTERN_SUBJECT_MAX_BYTES, ScriptContext,
    ScriptError, ScriptLimits, ScriptValue, SourceIssue,
};
use indexmap::IndexMap;

fn eval(code: &str) -> Result<ScriptValue, ScriptError> {
    LuauSandbox::default().eval(code, &ScriptContext::new())
}

fn eval_with(limits: ScriptLimits, code: &str) -> Result<ScriptValue, ScriptError> {
    LuauSandbox::new(limits).eval(code, &ScriptContext::new())
}

fn s(v: &str) -> ScriptValue {
    ScriptValue::String(v.to_string())
}

#[track_caller]
fn assert_limit(result: Result<ScriptValue, ScriptError>, kind: LimitKind, max: u64) {
    match result {
        Err(ScriptError::LimitExceeded { limit, max: m }) => {
            assert_eq!((limit, m), (kind, max));
        }
        other => panic!("expected {kind:?} limit, got {other:?}"),
    }
}

#[track_caller]
fn assert_runtime(result: Result<ScriptValue, ScriptError>, expected: &str) {
    match result {
        Err(ScriptError::Runtime { message, .. }) => assert_eq!(message, expected),
        other => panic!("expected runtime error {expected:?}, got {other:?}"),
    }
}

#[track_caller]
fn assert_type(result: Result<ScriptValue, ScriptError>, path: &str, message: &str) {
    match result {
        Err(ScriptError::Type {
            path: p,
            message: m,
        }) => {
            assert_eq!((p.as_str(), m.as_str()), (path, message));
        }
        other => panic!("expected type error at {path}, got {other:?}"),
    }
}

// ---------------------------------------------------------------- basic evaluation

#[test]
fn code_body_reads_injected_record_and_returns_action() {
    let mut record = IndexMap::new();
    record.insert("id".to_string(), ScriptValue::Int(42));
    record.insert("is_company".to_string(), ScriptValue::Bool(true));
    let mut ctx = ScriptContext::new();
    ctx.set_value("record", ScriptValue::Dict(record)).unwrap();
    ctx.capture("action").unwrap();

    let code = r#"
        if record.is_company then
            action = { type = "ir.actions.act_url", url = "https://example.com/p/" .. record.id }
        end
    "#;
    let out = LuauSandbox::default().run(code.as_bytes(), &ctx).unwrap();
    assert_eq!(out.value, ScriptValue::Nil, "no hidden `action` fallback");
    let mut action = IndexMap::new();
    action.insert("type".to_string(), s("ir.actions.act_url"));
    action.insert("url".to_string(), s("https://example.com/p/42"));
    assert_eq!(out.captured.get("action"), Some(&ScriptValue::Dict(action)));
}

#[test]
fn arithmetic_strings_and_libraries() {
    assert_eq!(eval("return 1 + 2 * 3").unwrap(), ScriptValue::Int(7));
    assert_eq!(eval("return 7 / 2").unwrap(), ScriptValue::Float(3.5));
    assert_eq!(eval("return math.sqrt(16)").unwrap(), ScriptValue::Int(4));
    assert_eq!(
        eval("return string.format('Hello %s, ID: %d', 'Partner', 42)").unwrap(),
        s("Hello Partner, ID: 42")
    );
    assert_eq!(eval("return ('erp'):upper()").unwrap(), s("ERP"));
    assert_eq!(
        eval("return bit32.band(6, 3)").unwrap(),
        ScriptValue::Int(2)
    );
    assert_eq!(
        eval("return utf8.len('Jürgen')").unwrap(),
        ScriptValue::Int(6)
    );
    assert_eq!(
        eval("return table.concat({'a', 'b', 1}, '-')").unwrap(),
        s("a-b-1")
    );
    assert_eq!(eval("").unwrap(), ScriptValue::Nil);
}

#[test]
fn non_ascii_and_crlf_sources() {
    assert_eq!(
        eval("local name = 'Jürgen ✓'\r\nreturn name\r\n").unwrap(),
        s("Jürgen ✓")
    );
    match eval("local x = 1\r\nif then end") {
        Err(ScriptError::Syntax { line, message }) => {
            assert_eq!(line, Some(2));
            assert_eq!(
                message,
                "Expected identifier when parsing expression, got 'then'"
            );
        }
        other => panic!("expected syntax error, got {other:?}"),
    }
}

#[test]
fn syntax_and_runtime_errors_are_structured() {
    match eval("if then return") {
        Err(e @ ScriptError::Syntax { .. }) => {
            assert_eq!(
                e.to_string(),
                "syntax error on line 1: Expected identifier when parsing expression, got 'then'"
            );
        }
        other => panic!("expected syntax error, got {other:?}"),
    }
    match eval("local a = 1\nerror('custom user error')") {
        Err(ScriptError::Runtime { message, traceback }) => {
            assert_eq!(message, "script:2: custom user error");
            let traceback = traceback.expect("a traceback");
            assert!(traceback.starts_with("stack traceback:"), "{traceback}");
        }
        other => panic!("expected runtime error, got {other:?}"),
    }
    assert_runtime(
        eval("return nil + 1"),
        "script:1: attempt to perform arithmetic (add) on nil and number",
    );
}

// ---------------------------------------------------------------- 1. bytecode

#[test]
fn compiled_bytecode_is_rejected_before_loading() {
    // Luau 0.740 bytecode starts with version byte 0x09 (a tab, which source may contain) and a
    // type-version byte 1..=3, so the check must look past the first byte.
    let bytecode = mlua::chunk::Compiler::new().compile("return 1").unwrap();
    let (at, byte) = bytecode
        .iter()
        .copied()
        .enumerate()
        .find(|&(_, b)| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
        .expect("bytecode contains control bytes");
    assert!(at <= 1, "the header itself is rejected, offset {at}");
    match LuauSandbox::default().eval_bytes(&bytecode, &ScriptContext::new()) {
        Err(ScriptError::InvalidSource { issue, offset }) => {
            let expected = if byte == 0 {
                SourceIssue::Nul
            } else {
                SourceIssue::ControlCharacter(byte)
            };
            assert_eq!((issue, offset), (expected, at));
        }
        other => panic!("expected InvalidSource, got {other:?}"),
    }
}

#[test]
fn nul_control_and_invalid_utf8_sources_are_rejected() {
    let cases: [(&[u8], SourceIssue, usize, &str); 4] = [
        (
            b"return 1\0",
            SourceIssue::Nul,
            8,
            "invalid script source at byte 8: NUL byte (Luau bytecode is not accepted)",
        ),
        (
            b"\x1bLuaR",
            SourceIssue::ControlCharacter(0x1b),
            0,
            "invalid script source at byte 0: control byte 0x1b (Luau bytecode is not accepted)",
        ),
        (
            b"\n\x02return 1",
            SourceIssue::ControlCharacter(0x02),
            1,
            "invalid script source at byte 1: control byte 0x02 (Luau bytecode is not accepted)",
        ),
        (
            b"return '\xff'",
            SourceIssue::InvalidUtf8,
            8,
            "invalid script source at byte 8: invalid UTF-8",
        ),
    ];
    for (src, issue, offset, message) in cases {
        let err = LuauSandbox::default()
            .eval_bytes(src, &ScriptContext::new())
            .unwrap_err();
        assert_eq!(err.to_string(), message);
        match err {
            ScriptError::InvalidSource {
                issue: i,
                offset: o,
            } => assert_eq!((i, o), (issue, offset)),
            other => panic!("expected InvalidSource, got {other:?}"),
        }
    }
}

#[test]
fn oversized_source_is_rejected() {
    let limits = ScriptLimits::default().with_source_bytes(16).unwrap();
    assert_limit(
        eval_with(limits, "return 1 + 2 + 3 + 4"),
        LimitKind::SourceBytes,
        16,
    );
    assert_eq!(eval_with(limits, "return 1").unwrap(), ScriptValue::Int(1));
}

// ---------------------------------------------------------------- 2. sticky, out-of-band quotas

#[test]
fn pcall_cannot_swallow_the_step_limit() {
    let limits = ScriptLimits::default().with_steps(10_000).unwrap();
    let code = r#"
        local ok = pcall(function() while true do end end)
        return "escaped"
    "#;
    assert_limit(eval_with(limits, code), LimitKind::Steps, 10_000);
}

#[test]
fn nested_pcall_loop_cannot_swallow_the_step_limit() {
    let limits = ScriptLimits::default().with_steps(10_000).unwrap();
    let code = r#"
        while true do
            pcall(function() while true do end end)
        end
    "#;
    assert_limit(eval_with(limits, code), LimitKind::Steps, 10_000);
}

#[test]
fn pcall_cannot_swallow_the_memory_limit() {
    let limits = ScriptLimits::default()
        .with_memory_bytes(1024 * 1024)
        .unwrap();
    let code = r#"
        local ok, err = pcall(function() return table.create(4000000, true) end)
        return "escaped"
    "#;
    assert_limit(eval_with(limits, code), LimitKind::Memory, 1024 * 1024);
}

#[test]
fn forged_limit_messages_stay_runtime_errors() {
    assert_runtime(
        eval("error('step limit of 1000 steps exceeded')"),
        "script:1: step limit of 1000 steps exceeded",
    );
    assert_runtime(
        eval("error('evaluation stopped')"),
        "script:1: evaluation stopped",
    );
    assert_runtime(
        eval("error('Instruction limit of 1000000 steps exceeded', 0)"),
        "Instruction limit of 1000000 steps exceeded",
    );
    // pcall still catches ordinary errors, with the message as the value.
    assert_eq!(
        eval("local ok, e = pcall(error, 'boom', 0) return {ok, e}").unwrap(),
        ScriptValue::List(vec![ScriptValue::Bool(false), s("boom")])
    );
    assert_eq!(
        eval("local ok, e = pcall(function() error('boom') end) return e").unwrap(),
        s("script:1: boom")
    );
    assert_eq!(
        eval("return {pcall(function(a, b) return a + b end, 2, 3)}").unwrap(),
        ScriptValue::List(vec![ScriptValue::Bool(true), ScriptValue::Int(5)])
    );
    assert_eq!(
        eval("return {pcall(nil)}").unwrap(),
        ScriptValue::List(vec![
            ScriptValue::Bool(false),
            s("attempt to call a nil value")
        ])
    );
}

#[test]
fn host_callbacks_fail_once_tripped() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let mut ctx = ScriptContext::new();
    ctx.set_fn("ping", move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(ScriptValue::Nil)
    })
    .unwrap();
    let limits = ScriptLimits::default().with_steps(5_000).unwrap();
    let code = r#"
        pcall(function() while true do end end)
        local ok = pcall(ping)
        ping()
        return "escaped"
    "#;
    let result = LuauSandbox::new(limits).eval(code, &ctx);
    assert_limit(result, LimitKind::Steps, 5_000);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn step_limit_stops_an_infinite_loop() {
    let limits = ScriptLimits::default().with_steps(5_000).unwrap();
    let err = eval_with(limits, "while true do end").unwrap_err();
    assert_eq!(err.to_string(), "step limit of 5000 steps exceeded");
    assert!(matches!(
        err,
        ScriptError::LimitExceeded {
            limit: LimitKind::Steps,
            max: 5_000
        }
    ));
}

/// Steps stay at their maximum; only the clock can stop this script.
#[test]
fn wall_clock_limit_trips_independently_of_steps() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("nap", |_| {
        std::thread::sleep(Duration::from_millis(5));
        Ok(ScriptValue::Nil)
    })
    .unwrap();
    let limits = ScriptLimits::default()
        .with_time(Duration::from_millis(50))
        .unwrap();
    let started = Instant::now();
    let result = LuauSandbox::new(limits).eval("while true do nap() end", &ctx);
    assert_limit(result, LimitKind::Time, 50);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// Steps and time stay at their maxima and the script runs a handful of steps; only memory can
/// stop it.
#[test]
fn memory_limit_trips_on_allocation_alone() {
    let limits = ScriptLimits::default()
        .with_memory_bytes(1024 * 1024)
        .unwrap();
    assert_limit(
        eval_with(limits, "local t = table.create(2000000, true) return #t"),
        LimitKind::Memory,
        1024 * 1024,
    );
    assert_eq!(
        eval_with(limits, "local t = table.create(1000, true) return #t").unwrap(),
        ScriptValue::Int(1000)
    );
}

// ---------------------------------------------------------------- 3. conversion guards

#[test]
fn cyclic_tables_are_type_errors() {
    assert_type(
        eval("local t = {} t.me = t return t"),
        "$.me",
        "cyclic table reference",
    );
    assert_type(
        eval("local a, b = {}, {} a.b = b b.a = a return a"),
        "$.b.a",
        "cyclic table reference",
    );
    assert_type(
        eval("local t = {1, 2} t[3] = {t} return t"),
        "$[3][1]",
        "cyclic table reference",
    );
    // Shared but acyclic sub-tables are fine.
    let shared = ScriptValue::List(vec![ScriptValue::Int(1)]);
    assert_eq!(
        eval("local a = {1} return {a, a}").unwrap(),
        ScriptValue::List(vec![shared.clone(), shared])
    );
}

#[test]
fn deep_tables_hit_the_depth_limit() {
    let code = r#"
        local root = {}
        local cur = root
        for i = 1, 10000 do cur.n = {} cur = cur.n end
        return root
    "#;
    assert_limit(eval(code), LimitKind::Depth, 64);
    let limits = ScriptLimits::default().with_depth(3).unwrap();
    assert_limit(eval_with(limits, "return {{{{}}}}"), LimitKind::Depth, 3);
    assert_eq!(
        eval_with(limits, "return {{{}}}").unwrap(),
        ScriptValue::List(vec![ScriptValue::List(vec![ScriptValue::Dict(
            IndexMap::new()
        )])])
    );
}

#[test]
fn deep_tables_passed_to_host_callbacks_hit_the_depth_limit() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("take", |_| Ok(ScriptValue::Nil)).unwrap();
    let code = r#"
        local root = {}
        local cur = root
        for i = 1, 10000 do cur[1] = {} cur = cur[1] end
        take(root)
    "#;
    assert_limit(
        LuauSandbox::default().eval(code, &ctx),
        LimitKind::Depth,
        64,
    );
    assert_type(
        LuauSandbox::default().eval("local t = {} t.t = t take(1, t)", &ctx),
        "take argument 2.t",
        "cyclic table reference",
    );
}

#[test]
fn huge_tables_hit_the_value_count_limit() {
    assert_limit(
        eval("return table.create(200000, 1)"),
        LimitKind::Nodes,
        100_000,
    );
    let limits = ScriptLimits::default().with_nodes(5).unwrap();
    assert_limit(eval_with(limits, "return {1, 2, 3}"), LimitKind::Nodes, 5);
    assert_eq!(
        eval_with(limits, "return {1, 2}").unwrap(),
        ScriptValue::List(vec![ScriptValue::Int(1), ScriptValue::Int(2)])
    );
}

/// Review round 3, item 2: one 8 MB Luau string referenced 100,000 times is 1.6 MB in the VM but
/// would be 800 GB of host strings. The exact script stops at the value-count limit; below it, the
/// byte budget (= memory limit, shared by the result and captured globals) stops the copies.
#[test]
fn repeated_strings_cannot_amplify_host_memory() {
    let started = Instant::now();
    assert_limit(
        eval(r#"local s=("x"):rep(8e6); local t={}; for i=1,100000 do t[i]=s end; return t"#),
        LimitKind::Nodes,
        100_000,
    );
    let budget = ScriptLimits::MAX_MEMORY_BYTES as u64;
    assert_limit(
        eval(r#"local s=("x"):rep(8e6); local t={}; for i=1,1000 do t[i]=s end; return t"#),
        LimitKind::ConvertedBytes,
        budget,
    );
    // A big key at every level.
    assert_limit(
        eval(
            r#"local k = ("k"):rep(4e6) local root = {} local cur = root
               for i = 1, 10 do local n = {} cur[k] = n cur = n end return root"#,
        ),
        LimitKind::ConvertedBytes,
        budget,
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // Result and captured globals share one budget.
    let mut ctx = ScriptContext::new();
    ctx.capture("copy").unwrap();
    let result = LuauSandbox::default().run(br#"local s = ("x"):rep(9e6) copy = s return s"#, &ctx);
    match result {
        Err(ScriptError::LimitExceeded { limit, max }) => {
            assert_eq!((limit, max), (LimitKind::ConvertedBytes, budget));
        }
        other => panic!("expected the converted-size limit, got {other:?}"),
    }

    // Host-callback arguments are charged too.
    let mut ctx = ScriptContext::new();
    ctx.set_fn("take", |_| Ok(ScriptValue::Nil)).unwrap();
    let err = LuauSandbox::default()
        .eval(
            r#"local s=("x"):rep(8e6); local t={}; for i=1,1000 do t[i]=s end; take(t)"#,
            &ctx,
        )
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("converted size limit of {budget} bytes exceeded")
    );
}

#[test]
fn long_keys_are_cut_in_error_paths() {
    let err = eval(r#"local t = {[("k"):rep(1000)] = function() end} return t"#).unwrap_err();
    match err {
        ScriptError::Type { path, message } => {
            assert_eq!(path, format!("$[{:?}...]", "k".repeat(64)));
            assert_eq!(message, "a function cannot be passed to the host");
        }
        other => panic!("expected a type error, got {other:?}"),
    }
}

// ---------------------------------------------------------------- 4. globals allowlist

#[test]
fn absent_globals_are_nil() {
    for name in [
        "require",
        "load",
        "loadstring",
        "dofile",
        "loadfile",
        "getfenv",
        "setfenv",
        "newproxy",
        "collectgarbage",
        "gcinfo",
        "print",
        "debug",
        "os",
        "io",
        "package",
        "coroutine",
        "buffer",
        "getmetatable",
        "setmetatable",
        "xpcall",
        "_G",
        "_VERSION",
    ] {
        let code = format!("return {name} == nil");
        assert_eq!(
            eval(&code).unwrap(),
            ScriptValue::Bool(true),
            "{name} must be absent"
        );
    }
    assert_runtime(
        eval("return require('os')"),
        "script:1: attempt to call a nil value",
    );
}

#[test]
fn environment_and_libraries_are_locked() {
    assert_runtime(
        eval("string.upper = nil"),
        "script:1: attempt to modify a readonly table",
    );
    assert_runtime(
        eval("table.insert = print"),
        "script:1: attempt to modify a readonly table",
    );
    assert_runtime(
        eval("local mt = rawget(string, 'x') rawset(math, 'pi', 3)"),
        "attempt to modify a readonly table",
    );
    // A script may shadow a builtin with its own global; the original stays intact elsewhere.
    assert_eq!(
        eval("local f = string.upper string = nil return f('a')").unwrap(),
        s("A")
    );
    assert_eq!(eval("x = 41 return x + 1").unwrap(), ScriptValue::Int(42));
}

#[test]
fn context_names_are_validated() {
    let mut ctx = ScriptContext::new();
    ctx.set_value("record", 1).unwrap();
    let cases: [(&str, &str); 7] = [
        ("record", "already defined"),
        ("string", "a sandbox builtin"),
        ("require", "deliberately absent from the sandbox"),
        ("end", "a Luau keyword"),
        ("1abc", "not an identifier ([A-Za-z_][A-Za-z0-9_]*)"),
        ("record.id", "the namespace is already defined as a value"),
        ("env.a.b", "only one namespace level is supported"),
    ];
    for (name, reason) in cases {
        let err = ctx.set_value(name, 2).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("invalid script name '{name}': {reason}")
        );
        assert!(matches!(err, ScriptError::InvalidName { .. }));
    }
    let err = ctx.capture("record").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid script name 'record': already injected; a captured global must be script-owned"
    );
}

#[test]
fn namespaced_host_functions() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("env.double", |args| match args {
        [ScriptValue::Int(n)] => Ok(ScriptValue::Int(n * 2)),
        _ => Err(HostError::new("expected one integer")),
    })
    .unwrap()
    .set_value("env.uid", 7)
    .unwrap();
    assert_eq!(
        LuauSandbox::default()
            .eval("return env.double(env.uid)", &ctx)
            .unwrap(),
        ScriptValue::Int(14)
    );
    assert_runtime(
        LuauSandbox::default().eval("env.double = nil", &ctx),
        "script:1: attempt to modify a readonly table",
    );
}

// ---------------------------------------------------------------- 5. bounded C-side work

#[test]
fn pattern_functions_cap_subject_and_pattern() {
    let over = PATTERN_SUBJECT_MAX_BYTES + 1;
    for call in [
        "string.find(s, 'a')",
        "s:find('a')",
        "string.match(s, 'a')",
        "s:gmatch('a')",
        "s:gsub('a', 'b')",
    ] {
        let name = call
            .split(['.', ':'])
            .nth(1)
            .and_then(|rest| rest.split('(').next())
            .unwrap();
        let code = format!("local s = string.rep('a', {over}) return {call}");
        assert_runtime(
            eval(&code),
            &format!(
                "string.{name}: subject is {over} bytes; the limit is {PATTERN_SUBJECT_MAX_BYTES} bytes"
            ),
        );
    }
    let at_cap = format!(
        "local s = string.rep('a', {PATTERN_SUBJECT_MAX_BYTES}) return (s:find('a', 1, true))"
    );
    assert_eq!(eval(&at_cap).unwrap(), ScriptValue::Int(1));

    let long = PATTERN_MAX_BYTES + 1;
    assert_runtime(
        eval(&format!(
            "return string.find('abc', string.rep('a', {long}))"
        )),
        &format!("string.find: pattern is {long} bytes; the limit is {PATTERN_MAX_BYTES} bytes"),
    );
}

/// A backtracking pattern inside the caps is stopped by the step limit (Luau calls the interrupt
/// from the matcher).
#[test]
fn backtracking_pattern_is_bounded_by_steps() {
    let limits = ScriptLimits::default().with_steps(100_000).unwrap();
    let code = format!(
        "local s = string.rep('a', {PATTERN_SUBJECT_MAX_BYTES}) return s:find(string.rep('a-', 20) .. 'b')"
    );
    assert_limit(eval_with(limits, &code), LimitKind::Steps, 100_000);
}

/// Review round 3, item 1: `("a*"):rep(100) .. "b"` against 64 KiB of `a` backtracks about n^100
/// times inside one C call. Luau's `match()` calls the interrupt on every recursion
/// (`lstrlib.cpp:430-443`), so with every limit at its maximum the call ends with a limit error
/// within the wall-clock limit plus slack.
#[test]
fn catastrophic_pattern_ends_within_the_wall_clock_limit() {
    let started = Instant::now();
    let result = eval(r#"return string.find(("a"):rep(65536), ("a*"):rep(100) .. "b")"#);
    let elapsed = started.elapsed();
    eprintln!("exact call: {result:?} after {elapsed:?}");
    match result {
        Err(ScriptError::LimitExceeded {
            limit: LimitKind::Time | LimitKind::Steps,
            ..
        }) => {}
        other => panic!("expected a time or step limit, got {other:?}"),
    }
    assert!(
        elapsed < ScriptLimits::MAX_TIME + Duration::from_millis(500),
        "took {elapsed:?}"
    );
}

#[test]
fn catastrophic_pattern_variants_end_within_a_lowered_wall_clock_limit() {
    let limit = Duration::from_millis(300);
    let limits = ScriptLimits::default().with_time(limit).unwrap();
    for call in [
        r#"s:find(p)"#,
        r#"string.match(s, p)"#,
        r#"(s:gsub(p, "x"))"#,
        r#"(function() for m in s:gmatch(p) do end end)()"#,
    ] {
        let code =
            format!(r#"local s, p = ("a"):rep(65536), ("a*"):rep(100) .. "b" return {call}"#);
        let started = Instant::now();
        let result = eval_with(limits, &code);
        let elapsed = started.elapsed();
        eprintln!("{call}: {result:?} after {elapsed:?}");
        match result {
            Err(ScriptError::LimitExceeded {
                limit: LimitKind::Time | LimitKind::Steps,
                ..
            }) => {}
            other => panic!("{call}: expected a time or step limit, got {other:?}"),
        }
        assert!(
            elapsed < limit + Duration::from_millis(500),
            "{call}: took {elapsed:?}"
        );
    }
}

#[test]
fn string_rep_and_table_concat_charge_memory_first() {
    let limits = ScriptLimits::default()
        .with_memory_bytes(4 * 1024 * 1024)
        .unwrap();
    assert_limit(
        eval_with(limits, "return #string.rep('x', 8 * 1024 * 1024)"),
        LimitKind::Memory,
        4 * 1024 * 1024,
    );
    assert_limit(
        eval_with(
            limits,
            "local ok = pcall(function() return ('x'):rep(8 * 1024 * 1024) end) return 1",
        ),
        LimitKind::Memory,
        4 * 1024 * 1024,
    );
    let concat = r#"
        local t = table.create(1000, 'abc')
        local sep = string.rep('y', 8192)
        return #table.concat(t, sep)
    "#;
    assert_limit(
        eval_with(limits, concat),
        LimitKind::Memory,
        4 * 1024 * 1024,
    );
    assert_eq!(
        eval_with(limits, "return #string.rep('ab', 1000)").unwrap(),
        ScriptValue::Int(2000)
    );
}

#[test]
fn table_move_charges_steps() {
    let limits = ScriptLimits::default().with_steps(10_000).unwrap();
    assert_limit(
        eval_with(limits, "table.move({}, 1, 1000000, 1, {})"),
        LimitKind::Steps,
        10_000,
    );
}

/// Luau 0.740 `str_rep` returns at once for an empty string (the doubling loop never runs), so a
/// huge count costs nothing; checked with a tiny step budget.
#[test]
fn string_rep_of_empty_string_is_constant_time() {
    let limits = ScriptLimits::default().with_steps(100).unwrap();
    let started = Instant::now();
    assert_eq!(
        eval_with(limits, "return #string.rep('', 2147483647)").unwrap(),
        ScriptValue::Int(0)
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn table_sort_charges_n_log_n_steps() {
    let limits = ScriptLimits::default().with_steps(1_000_000).unwrap();
    assert_limit(
        eval_with(
            limits,
            "local t = table.create(500000, 1) table.sort(t) return 1",
        ),
        LimitKind::Steps,
        1_000_000,
    );
    assert_eq!(
        eval_with(limits, "local t = {3, 1, 2} table.sort(t) return t").unwrap(),
        ScriptValue::List(vec![
            ScriptValue::Int(1),
            ScriptValue::Int(2),
            ScriptValue::Int(3)
        ])
    );
    assert_eq!(
        eval("local t = {1, 3, 2} table.sort(t, function(a, b) return a > b end) return t")
            .unwrap(),
        ScriptValue::List(vec![
            ScriptValue::Int(3),
            ScriptValue::Int(2),
            ScriptValue::Int(1)
        ])
    );
}

#[test]
fn reference_values_never_print_heap_addresses() {
    assert_eq!(
        eval(
            "return {tostring({}), tostring(tostring), tostring(12), tostring(nil), tostring(true)}"
        )
        .unwrap(),
        ScriptValue::List(vec![
            s("table"),
            s("function"),
            s("12"),
            s("nil"),
            s("true")
        ])
    );
    assert_eq!(
        eval("return string.format('%s|%s|%d', {}, pairs, 7)").unwrap(),
        s("table|function|7")
    );
    assert_eq!(
        eval("local ok, e = pcall(error, {}) return e").unwrap(),
        s("table")
    );
    assert_runtime(eval("error({})"), "script:1: table");
    assert_runtime(eval("error(function() end, 0)"), "function");
}

#[test]
fn rust_mediated_nesting_is_bounded() {
    let code = r#"
        local reached, msg = 0, nil
        local function f(n)
            reached = n
            local ok, e = pcall(f, n + 1)
            if not ok and msg == nil then msg = e end
        end
        f(1)
        return {reached, msg}
    "#;
    assert_eq!(
        eval(code).unwrap(),
        ScriptValue::List(vec![
            ScriptValue::Int(33),
            s("pcall: nesting limit of 32 exceeded")
        ])
    );
}

// ---------------------------------------------------------------- 6. no lossy conversions

#[test]
fn lossy_results_are_type_errors() {
    assert_type(
        eval("return function() end"),
        "$",
        "a function cannot be passed to the host",
    );
    assert_type(
        eval("return {1, nil, 3}"),
        "$",
        "array has a hole: index 3 in a table of 2 entries (keys must be 1..=2)",
    );
    assert_type(
        eval("return {1, x = 2}"),
        "$",
        "table mixes string keys with a key of type number (1)",
    );
    assert_type(
        eval("return {x = 1, [5] = 2}"),
        "$",
        "table mixes string keys with a key of type number (5)",
    );
    assert_type(
        eval("return {[true] = 1}"),
        "$",
        "unsupported table key of type boolean (true)",
    );
    assert_type(
        eval("return {[1.5] = 1}"),
        "$",
        "unsupported table key of type number (1.5)",
    );
    assert_type(
        eval("return {['\\xff'] = 1}"),
        "$",
        "table key is not valid UTF-8",
    );
    assert_type(
        eval("return 0/0"),
        "$",
        "number NaN has no host representation",
    );
    assert_type(
        eval("return {x = {1/0}}"),
        "$.x[1]",
        "number inf has no host representation",
    );
    assert_type(
        eval("return 1, 2"),
        "$",
        "the script returned 2 values; return one value (a table for several)",
    );
}

#[test]
fn exact_conversions_round_trip() {
    assert_eq!(
        eval("return '\\xff\\x00'").unwrap(),
        ScriptValue::Bytes(vec![0xff, 0x00])
    );
    assert_eq!(eval("return 2^53").unwrap(), ScriptValue::Int(1 << 53));
    assert_eq!(
        eval("return 2^63").unwrap(),
        ScriptValue::Float(2f64.powi(63))
    );
    assert_eq!(
        eval("return {}").unwrap(),
        ScriptValue::Dict(IndexMap::new())
    );
    let mut sorted = IndexMap::new();
    sorted.insert("a".to_string(), ScriptValue::Int(1));
    sorted.insert("b".to_string(), ScriptValue::Int(2));
    assert_eq!(
        eval("return {b = 2, a = 1}").unwrap(),
        ScriptValue::Dict(sorted)
    );

    let mut ctx = ScriptContext::new();
    ctx.set_value("big", ScriptValue::Int((1 << 53) + 1))
        .unwrap();
    assert_type(
        LuauSandbox::default().eval("return big", &ctx),
        "big",
        "integer 9007199254740993 cannot be represented exactly in Luau (limit is ±2^53)",
    );
    let mut ctx = ScriptContext::new();
    ctx.set_value("min", ScriptValue::Int(i64::MIN)).unwrap();
    assert_type(
        LuauSandbox::default().eval("return min", &ctx),
        "min",
        "integer -9223372036854775808 cannot be represented exactly in Luau (limit is ±2^53)",
    );
}

#[test]
fn script_value_accessors_and_json_are_exact() {
    let err = ScriptValue::Float(2f64.powi(63)).as_int().unwrap_err();
    assert_eq!(
        err.to_string(),
        "type error at $: expected integer, got float 9223372036854776000"
    );
    assert_eq!(ScriptValue::Float(3.0).as_int().unwrap(), 3);
    assert_eq!(
        ScriptValue::Int(i64::MIN)
            .as_float()
            .unwrap_err()
            .to_string(),
        "type error at $: expected float, got integer -9223372036854775808"
    );

    let nan = serde_json::Value::try_from(ScriptValue::Float(f64::NAN)).unwrap_err();
    assert_eq!(
        nan.to_string(),
        "type error at $: float NaN has no JSON representation"
    );
    let bytes = serde_json::Value::try_from(ScriptValue::Bytes(vec![0xff])).unwrap_err();
    assert_eq!(
        bytes.to_string(),
        "type error at $: 1 bytes that are not UTF-8 have no JSON representation"
    );
    let big: serde_json::Value = serde_json::from_str("18446744073709551615").unwrap();
    assert_eq!(
        ScriptValue::try_from(big).unwrap_err().to_string(),
        "type error at $: JSON integer 18446744073709551615 does not fit a 64-bit signed integer"
    );
    let ok: serde_json::Value =
        serde_json::from_str(r#"{"a": [1, 2.5, "x", null, true]}"#).unwrap();
    let back = serde_json::Value::try_from(ScriptValue::try_from(ok.clone()).unwrap()).unwrap();
    assert_eq!(back, ok);
}

// ---------------------------------------------------------------- 7. host callback errors

#[derive(Debug)]
struct AccessDenied {
    model: String,
}

impl std::fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no read access on {} (secret detail)", self.model)
    }
}

impl std::error::Error for AccessDenied {}

#[test]
fn host_errors_keep_their_kind_and_stay_opaque_to_scripts() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("env.search", |_| {
        Err(AccessDenied {
            model: "res.partner".to_string(),
        }
        .into())
    })
    .unwrap();
    ctx.capture("seen").unwrap();
    // The script catches the failure and tries to carry on; the evaluation still fails with the
    // host's error, and what the script saw is the fixed message.
    let code = r#"
        local ok, e = pcall(env.search, 'res.partner')
        seen = e
        return "carried on"
    "#;
    let err = LuauSandbox::default()
        .run(code.as_bytes(), &ctx)
        .unwrap_err();
    assert_eq!(err.to_string(), "host function 'env.search' failed");
    let source = std::error::Error::source(&err).expect("source");
    let denied = source.downcast_ref::<AccessDenied>().expect("AccessDenied");
    assert_eq!(denied.model, "res.partner");
    match err {
        ScriptError::Host { function, .. } => assert_eq!(function, "env.search"),
        other => panic!("expected Host, got {other:?}"),
    }
}

#[test]
fn host_error_message_seen_by_script_is_fixed() {
    // Run the failing call with no other way out than `error(e)`: still the host error, and the
    // secret detail never becomes part of a runtime error.
    let mut ctx = ScriptContext::new();
    ctx.set_fn("fail", |_| Err(HostError::new("secret detail")))
        .unwrap();
    let err = LuauSandbox::default()
        .eval("local ok, e = pcall(fail) error(e)", &ctx)
        .unwrap_err();
    assert!(matches!(err, ScriptError::Host { .. }), "{err:?}");
    assert_eq!(err.to_string(), "host function 'fail' failed");
}

#[test]
fn host_type_errors_keep_their_kind() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("want_int", |args| match args.first() {
        Some(v) => Ok(ScriptValue::Int(v.as_int()?)),
        None => Err(HostError::new("missing argument")),
    })
    .unwrap();
    let err = LuauSandbox::default()
        .eval("return want_int('x')", &ctx)
        .unwrap_err();
    let inner = std::error::Error::source(&err)
        .and_then(|e| e.downcast_ref::<ScriptError>())
        .expect("ScriptError source");
    assert_eq!(
        inner.to_string(),
        "type error at $: expected integer, got string"
    );

    // A function passed to a host callback is a type error of the evaluation.
    assert_type(
        LuauSandbox::default().eval("return want_int(print or function() end)", &ctx),
        "want_int argument 1",
        "a function cannot be passed to the host",
    );
}

#[test]
fn host_panics_become_errors() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("boom", |_| -> Result<ScriptValue, HostError> {
        panic!("kaboom")
    })
    .unwrap();
    let err = LuauSandbox::default()
        .eval("pcall(boom) return 1", &ctx)
        .unwrap_err();
    assert_eq!(err.to_string(), "panic in host function 'boom': kaboom");
    assert!(matches!(err, ScriptError::Panic { .. }));
}

#[test]
fn host_results_are_bounded_and_exact() {
    let mut ctx = ScriptContext::new();
    ctx.set_fn("huge", |_| Ok(ScriptValue::Int(i64::MAX)))
        .unwrap();
    assert_type(
        LuauSandbox::default().eval("return huge()", &ctx),
        "huge result",
        "integer 9223372036854775807 cannot be represented exactly in Luau (limit is ±2^53)",
    );
}

// ---------------------------------------------------------------- 8. limits validation

#[test]
fn zero_and_oversized_limits_are_rejected() {
    let d = ScriptLimits::default();
    let cases: Vec<(Result<ScriptLimits, ScriptError>, &str)> = vec![
        (
            d.with_steps(0),
            "invalid step limit 0 steps: must be between 1 and 10000000",
        ),
        (
            d.with_steps(10_000_001),
            "invalid step limit 10000001 steps: must be between 1 and 10000000",
        ),
        (
            d.with_time(Duration::ZERO),
            "invalid time limit 0 ms: must be between 1 and 5000",
        ),
        (
            d.with_time(Duration::from_secs(6)),
            "invalid time limit 6000 ms: must be between 1 and 5000",
        ),
        (
            d.with_memory_bytes(0),
            "invalid memory limit 0 bytes: must be between 1 and 16777216",
        ),
        (
            d.with_memory_bytes(16 * 1024 * 1024 + 1),
            "invalid memory limit 16777217 bytes: must be between 1 and 16777216",
        ),
        (
            d.with_source_bytes(0),
            "invalid source size limit 0 bytes: must be between 1 and 262144",
        ),
        (
            d.with_depth(65),
            "invalid nesting depth limit 65 levels: must be between 1 and 64",
        ),
        (
            d.with_nodes(100_001),
            "invalid value count limit 100001 values: must be between 1 and 100000",
        ),
    ];
    for (result, message) in cases {
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), message);
        assert!(matches!(err, ScriptError::InvalidLimit { .. }));
    }
    let lowered = d
        .with_steps(1)
        .unwrap()
        .with_time(Duration::from_millis(1))
        .unwrap();
    assert_eq!(
        (lowered.steps(), lowered.time()),
        (1, Duration::from_millis(1))
    );
    assert_eq!(d, ScriptLimits::default());
    assert_eq!(d.steps(), ScriptLimits::MAX_STEPS);
}

// ---------------------------------------------------------------- isolation and concurrency

#[test]
fn evaluations_are_isolated() {
    let sandbox = LuauSandbox::default();
    let ctx = ScriptContext::new();
    sandbox.eval("leak = 1", &ctx).unwrap();
    assert_eq!(sandbox.eval("return leak", &ctx).unwrap(), ScriptValue::Nil);
}

#[test]
fn shared_context_across_threads() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let mut ctx = ScriptContext::new();
    ctx.set_fn("inc", move |args| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(ScriptValue::Int(args[0].as_int()? + 1))
    })
    .unwrap();
    let ctx = Arc::new(ctx);
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let ctx = Arc::clone(&ctx);
            std::thread::spawn(move || {
                LuauSandbox::default()
                    .eval(
                        &format!("local x = {i} for _ = 1, 100 do x = inc(x) end return x"),
                        &ctx,
                    )
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<ScriptValue> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results,
        (0..4)
            .map(|i| ScriptValue::Int(i + 100))
            .collect::<Vec<_>>()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 400);
}

/// Cost of one evaluation (fresh VM, environment, compile, run). Run with
/// `cargo test -p erp-script --release -- --ignored --nocapture vm_per_eval_cost`.
#[test]
#[ignore = "measurement, not a check"]
fn vm_per_eval_cost() {
    let sandbox = LuauSandbox::default();
    let ctx = ScriptContext::new();
    for _ in 0..50 {
        sandbox.eval("return 1", &ctx).unwrap();
    }
    let n = 2_000;
    let started = Instant::now();
    for _ in 0..n {
        sandbox.eval("return 1", &ctx).unwrap();
    }
    let per = started.elapsed() / n;
    println!("vm_per_eval_cost: {per:?} per evaluation over {n} runs");
}

/// Compilation runs outside the memory and step budgets (C++ allocations, no interrupts). Deep
/// nesting within the source cap must end in a syntax error from the parser's recursion limit, not a
/// native stack overflow, and a source at the cap must compile quickly.
#[test]
#[ignore = "DEFERRED_PERFORMANCE: milestone end"]
fn compilation_of_hostile_sources_is_bounded() {
    let depth = 100_000;
    for (open, close) in [("(", ")"), ("{", "}")] {
        let code = format!("return {}1{}", open.repeat(depth), close.repeat(depth));
        let started = Instant::now();
        let result = eval(&code);
        eprintln!("nested {open}: {result:?} after {:?}", started.elapsed());
        assert_recursion_limit(result);
    }
    // About 128k operands, just under the 256 KiB cap: the binary-operator parser recurses too.
    let wide = format!(
        "return 1{}",
        "+1".repeat(ScriptLimits::MAX_SOURCE_BYTES / 2 - 8)
    );
    assert_recursion_limit(eval(&wide));

    // 65,000 statements just under the cap compile and run quickly.
    let long = "x = 1\n".repeat(43_000);
    assert!(long.len() <= ScriptLimits::MAX_SOURCE_BYTES);
    let started = Instant::now();
    assert_eq!(eval(&long).unwrap(), ScriptValue::Nil);
    let elapsed = started.elapsed();
    eprintln!("43k statements: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[track_caller]
fn assert_recursion_limit(result: Result<ScriptValue, ScriptError>) {
    match result {
        Err(ScriptError::Syntax { line, message }) => {
            assert_eq!(line, Some(1));
            assert_eq!(
                message,
                "Exceeded allowed recursion depth; simplify your expression to make the code compile"
            );
        }
        other => panic!("expected the parser's recursion limit, got {other:?}"),
    }
}

/// Admission/recursion checks stay active independently of the compiler timing characterization.
#[test]
fn hostile_compilation_preserves_recursion_and_source_limits() {
    let depth = 100_000;
    for (open, close) in [("(", ")"), ("{", "}")] {
        let code = format!("return {}1{}", open.repeat(depth), close.repeat(depth));
        assert_recursion_limit(eval(&code));
    }
    let wide = format!(
        "return 1{}",
        "+1".repeat(ScriptLimits::MAX_SOURCE_BYTES / 2 - 8)
    );
    assert_recursion_limit(eval(&wide));
    let long = "x = 1\n".repeat(43_000);
    assert!(long.len() <= ScriptLimits::MAX_SOURCE_BYTES);
    assert_eq!(eval(&long).unwrap(), ScriptValue::Nil);
}

#[test]
fn host_cancellation_is_sticky_across_pcall() {
    let token = erp_script::ScriptCancellation::default();
    let cancel = token.clone();
    let mut ctx = ScriptContext::new();
    ctx.set_fn("cancel", move |_| {
        cancel.cancel();
        Ok(ScriptValue::Nil)
    })
    .unwrap();
    let error = LuauSandbox::default()
        .run_cancellable(b"pcall(cancel); return 42", &ctx, &token)
        .unwrap_err();
    assert!(matches!(error, ScriptError::Cancelled), "{error:?}");
}

#[test]
fn pre_cancelled_cpu_script_never_succeeds() {
    let token = erp_script::ScriptCancellation::default();
    token.cancel();
    let error = LuauSandbox::default()
        .run_cancellable(b"while true do end", &ScriptContext::new(), &token)
        .unwrap_err();
    assert!(matches!(error, ScriptError::Cancelled), "{error:?}");
}
