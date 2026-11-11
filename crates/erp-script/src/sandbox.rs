//! The sandboxed Luau evaluator.
//!
//! One fresh VM per evaluation. The script runs in a new environment table built from an explicit
//! allowlist, never in mlua's global table (which carries `require` with filesystem access,
//! `loadstring` and `collectgarbage`, installed by mlua for Luau). Sources load as text only.
//! Limits trip a sticky, out-of-band flag that `pcall` cannot clear and `error(...)` cannot set.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use indexmap::IndexMap;
use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value, VmState};

use crate::context::{Entry, HostFn, Member, ScriptContext};
use crate::convert::{Budget, Root, is_memory_error, lua_error, to_lua, to_script};
use crate::error::{LimitKind, ScriptError, ScriptResult, SourceIssue};
use crate::limits::{
    NATIVE_NESTING_MAX, PATTERN_MAX_BYTES, PATTERN_SUBJECT_MAX_BYTES, ScriptLimits,
};
use crate::value::ScriptValue;

/// Chunk name; Luau prefixes positions with it (`script:3: message`).
const CHUNK_NAME: &str = "=script";
const CHUNK_PREFIX: &str = "script:";
/// The only message a script sees once the evaluation has been stopped (limit or host failure).
const STOPPED: &str = "evaluation stopped";
/// Base functions copied from Luau into the script environment. `pcall` is the sandbox's own.
const BASE_FUNCTIONS: &[&str] = &[
    "assert", "error", "ipairs", "next", "pairs", "rawequal", "rawget", "rawlen", "rawset",
    "select", "tonumber", "tostring", "type", "typeof", "unpack",
];
/// Libraries copied (and frozen) into the script environment.
const LIBRARIES: &[&str] = &["math", "string", "table", "utf8", "bit32"];
/// Wall-clock is checked every this many steps in the interrupt (and on every host call).
const TIME_CHECK_INTERVAL: u64 = 64;
/// Bytes charged per number in `table.concat` (a formatted Luau number is at most this long).
const NUMBER_TEXT_BYTES: usize = 32;
/// Chunk name of the sandbox's own Luau helpers.
const HELPER_CHUNK_NAME: &str = "=sandbox";
/// `error` wrapper: error values are strings (or numbers, or nil). Any other value is replaced by
/// its type name, so no table or function reaches `tostring`-style formatting (which prints heap
/// addresses) when the message is built. Positions keep pointing at the caller: a positive level is
/// shifted past this wrapper's own frame.
const ERROR_WRAPPER: &str = r#"
local native_error, typeof = ...
return function(message, level)
    local kind = typeof(message)
    if kind ~= "string" and kind ~= "number" and kind ~= "nil" then
        message = kind
    end
    if level == nil then
        level = 1
    end
    if typeof(level) == "number" and level > 0 then
        level = level + 1
    end
    native_error(message, level)
end
"#;

/// Result of [`LuauSandbox::run`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Evaluation {
    /// The script's return value (`Nil` when it returns nothing).
    pub value: ScriptValue,
    /// Globals requested with [`ScriptContext::capture`], in request order.
    pub captured: IndexMap<String, ScriptValue>,
}

/// Host-owned sticky cancellation shared with a blocking evaluation. Scripts cannot reset it.
///
/// ```
/// use erp_script::ScriptCancellation;
/// let owner = ScriptCancellation::default();
/// let worker = owner.clone();
/// owner.cancel();
/// assert!(worker.is_cancelled());
/// ```
#[derive(Debug, Clone, Default)]
pub struct ScriptCancellation(Arc<AtomicBool>);

impl ScriptCancellation {
    /// Stop this evaluation at its next interrupt or host boundary (see [`Self`]'s example).
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Whether cancellation was requested (see [`Self`]'s example).
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Evaluates untrusted Luau under [`ScriptLimits`].
#[derive(Debug, Clone, Default)]
pub struct LuauSandbox {
    limits: ScriptLimits,
}

impl LuauSandbox {
    /// A sandbox with the given (validated) limits.
    #[must_use]
    pub fn new(limits: ScriptLimits) -> Self {
        Self { limits }
    }

    /// The active limits.
    pub fn limits(&self) -> &ScriptLimits {
        &self.limits
    }

    /// Evaluates `source` and returns its result.
    pub fn eval(&self, source: &str, ctx: &ScriptContext) -> ScriptResult<ScriptValue> {
        Ok(self.run(source.as_bytes(), ctx)?.value)
    }

    /// Evaluates raw source bytes (as read from an addon file or a database column). Bytes that are
    /// not UTF-8, NUL and other control bytes are rejected, so Luau bytecode never reaches the VM.
    pub fn eval_bytes(&self, source: &[u8], ctx: &ScriptContext) -> ScriptResult<ScriptValue> {
        Ok(self.run(source, ctx)?.value)
    }

    /// Evaluates `source` and returns its result and the captured globals.
    pub fn run(&self, source: &[u8], ctx: &ScriptContext) -> ScriptResult<Evaluation> {
        self.run_cancellable(source, ctx, &ScriptCancellation::default())
    }

    /// Evaluate with host cancellation. A cancellation is sticky and cannot be caught by `pcall`.
    /// Limits and the allowlist are identical to [`Self::run`].
    ///
    /// ```
    /// use erp_script::{LuauSandbox, ScriptCancellation, ScriptContext, ScriptValue};
    /// let stop = ScriptCancellation::default();
    /// let result = LuauSandbox::default().run_cancellable(
    ///     b"return 1", &ScriptContext::new(), &stop,
    /// )?;
    /// assert_eq!(result.value, ScriptValue::Int(1));
    /// # Ok::<(), erp_script::ScriptError>(())
    /// ```
    pub fn run_cancellable(
        &self,
        source: &[u8],
        ctx: &ScriptContext,
        cancellation: &ScriptCancellation,
    ) -> ScriptResult<Evaluation> {
        if cancellation.is_cancelled() {
            return Err(ScriptError::Cancelled);
        }
        let text = validate_source(source, &self.limits)?;
        match catch_unwind(AssertUnwindSafe(|| self.run_text(text, ctx, cancellation))) {
            Ok(result) => result,
            Err(payload) => Err(ScriptError::Panic {
                context: "evaluation".to_string(),
                message: panic_message(payload.as_ref()),
            }),
        }
    }

    fn run_text(
        &self,
        text: &str,
        ctx: &ScriptContext,
        cancellation: &ScriptCancellation,
    ) -> ScriptResult<Evaluation> {
        let limits = self.limits;
        let deadline = Instant::now()
            .checked_add(limits.time())
            .ok_or_else(|| ScriptError::engine("deadline overflows the clock"))?;
        let lua = Lua::new_with(
            StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::UTF8 | StdLib::BIT,
            LuaOptions::default(),
        )
        .map_err(ScriptError::engine)?;
        lua.set_memory_limit(limits.memory_bytes())
            .map_err(ScriptError::engine)?;

        let state = Rc::new(RunState {
            cancellation: cancellation.clone(),
            limits,
            steps: Cell::new(0),
            deadline,
            tripped: Cell::new(false),
            trip: RefCell::new(None),
            nesting: Cell::new(0),
        });
        let st = Rc::clone(&state);
        lua.set_interrupt(move |_| st.interrupt());

        let env = match build_env(&lua, &state, ctx) {
            Ok(env) => env,
            // A limit tripped while building is the cause; the build error is its consequence.
            Err(e) => match state.take_trip() {
                Some(trip) => return Err(trip),
                None => return Err(e),
            },
        };

        let func = lua
            .load(text)
            .set_name(CHUNK_NAME)
            .set_mode(ChunkMode::Text)
            .set_environment(env.clone())
            .into_function();
        let func = match func {
            Ok(f) => f,
            Err(e) => return Err(load_error(e, &limits)),
        };

        let outcome = func.call::<MultiValue>(());
        if let Some(trip) = state.take_trip() {
            return Err(trip);
        }
        let values = outcome.map_err(|e| call_error(e, &limits))?;

        // One byte budget for the result and every captured global.
        let mut budget = Budget::new(&limits);
        let value = match values.len() {
            0 => ScriptValue::Nil,
            1 => to_script(&values[0], &limits, Root::Result, &mut budget)?,
            n => {
                return Err(ScriptError::type_error(
                    "$",
                    format!(
                        "the script returned {n} values; return one value (a table for several)"
                    ),
                ));
            }
        };
        let mut captured = IndexMap::new();
        for name in ctx.captures() {
            let v: Value = env.raw_get(name.as_str()).map_err(ScriptError::engine)?;
            let converted = to_script(&v, &limits, Root::Named(name), &mut budget)?;
            captured.insert(name.clone(), converted);
        }
        Ok(Evaluation { value, captured })
    }
}

/// Per-evaluation state shared by the interrupt and every Rust callback.
struct RunState {
    cancellation: ScriptCancellation,
    limits: ScriptLimits,
    steps: Cell<u64>,
    deadline: Instant,
    tripped: Cell<bool>,
    /// The first failure that stopped the evaluation; later ones are ignored.
    trip: RefCell<Option<ScriptError>>,
    nesting: Cell<u32>,
}

impl RunState {
    fn trip(&self, err: ScriptError) {
        if !self.tripped.get() {
            self.tripped.set(true);
            *self.trip.borrow_mut() = Some(err);
        }
    }

    fn take_trip(&self) -> Option<ScriptError> {
        self.trip.borrow_mut().take()
    }

    fn stopped(&self) -> mlua::Error {
        mlua::Error::runtime(STOPPED)
    }

    fn ensure_running(&self) -> mlua::Result<()> {
        if self.cancellation.is_cancelled() {
            self.trip(ScriptError::Cancelled);
        }
        if self.tripped.get() {
            return Err(self.stopped());
        }
        Ok(())
    }

    fn charge(&self, n: u64) -> mlua::Result<()> {
        self.ensure_running()?;
        let steps = self.steps.get().saturating_add(n);
        self.steps.set(steps);
        if steps > self.limits.steps() {
            self.trip(self.limits.exceeded(LimitKind::Steps));
            return Err(self.stopped());
        }
        Ok(())
    }

    fn check_time(&self) -> mlua::Result<()> {
        self.ensure_running()?;
        if Instant::now() >= self.deadline {
            self.trip(self.limits.exceeded(LimitKind::Time));
            return Err(self.stopped());
        }
        Ok(())
    }

    fn interrupt(&self) -> mlua::Result<VmState> {
        self.charge(1)?;
        if self.steps.get().is_multiple_of(TIME_CHECK_INTERVAL) {
            self.check_time()?;
        }
        Ok(VmState::Continue)
    }

    /// Charges the memory budget for `bytes` about to be allocated by a library function.
    fn charge_memory(&self, lua: &Lua, bytes: Option<usize>) -> mlua::Result<()> {
        self.ensure_running()?;
        let fits = bytes
            .and_then(|b| b.checked_add(lua.used_memory()))
            .is_some_and(|total| total <= self.limits.memory_bytes());
        if !fits {
            self.trip(self.limits.exceeded(LimitKind::Memory));
            return Err(self.stopped());
        }
        Ok(())
    }

    /// Records an allocation failure seen in a nested call as the memory limit.
    fn relay<T>(&self, result: mlua::Result<T>) -> mlua::Result<T> {
        if let Err(e) = &result
            && is_memory_error(e)
        {
            self.trip(self.limits.exceeded(LimitKind::Memory));
        }
        result
    }

    fn enter(self: &Rc<Self>, what: &str) -> mlua::Result<NestGuard> {
        let depth = self.nesting.get() + 1;
        if depth > NATIVE_NESTING_MAX {
            return Err(mlua::Error::runtime(format!(
                "{what}: nesting limit of {NATIVE_NESTING_MAX} exceeded"
            )));
        }
        self.nesting.set(depth);
        Ok(NestGuard(Rc::clone(self)))
    }
}

struct NestGuard(Rc<RunState>);

impl Drop for NestGuard {
    fn drop(&mut self) {
        self.0.nesting.set(self.0.nesting.get().saturating_sub(1));
    }
}

/// Rejects oversized sources, invalid UTF-8, NUL and control bytes (except tab, LF, CR). Luau
/// bytecode starts with a version byte below 0x0c followed by a type-version byte 1..=3 (mlua-sys
/// `luaL_isbytecode`), so no accepted source can be taken for bytecode; the text-only load mode is
/// the second layer.
fn validate_source<'a>(source: &'a [u8], limits: &ScriptLimits) -> ScriptResult<&'a str> {
    if source.len() > limits.source_bytes() {
        return Err(limits.exceeded(LimitKind::SourceBytes));
    }
    let control = source
        .iter()
        .position(|&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'));
    let utf8 = std::str::from_utf8(source);
    let utf8_offset = utf8.as_ref().err().map(|e| e.valid_up_to());
    match (control, utf8_offset) {
        (Some(c), Some(u)) if u < c => Err(ScriptError::InvalidSource {
            issue: SourceIssue::InvalidUtf8,
            offset: u,
        }),
        (Some(c), _) => Err(ScriptError::InvalidSource {
            issue: if source[c] == 0 {
                SourceIssue::Nul
            } else {
                SourceIssue::ControlCharacter(source[c])
            },
            offset: c,
        }),
        (None, Some(u)) => Err(ScriptError::InvalidSource {
            issue: SourceIssue::InvalidUtf8,
            offset: u,
        }),
        (None, None) => utf8.map_err(ScriptError::engine),
    }
}

fn build_env(lua: &Lua, st: &Rc<RunState>, ctx: &ScriptContext) -> ScriptResult<Table> {
    let limits = st.limits;
    let e = |err: mlua::Error| lua_error(err, &limits);
    let globals = lua.globals();
    let base = lua.create_table().map_err(e)?;

    for name in BASE_FUNCTIONS {
        let value: Value = globals.raw_get(*name).map_err(e)?;
        let Value::Function(original) = value else {
            return Err(ScriptError::engine(format!(
                "Luau did not provide the builtin function '{name}'"
            )));
        };
        let installed = match *name {
            "tostring" => wrap_tostring(lua, original).map_err(e)?,
            "error" => wrap_error(lua, original, &globals).map_err(e)?,
            _ => original,
        };
        base.raw_set(*name, installed).map_err(e)?;
    }
    for name in LIBRARIES {
        let lib: Table = globals.raw_get(*name).map_err(e)?;
        match *name {
            // The string library is also the `__index` of the string metatable (`s:find(...)`), so
            // the wrappers replace the functions in that very table.
            "string" => wrap_string_library(lua, st, &lib).map_err(e)?,
            "table" => wrap_table_library(lua, st, &lib).map_err(e)?,
            // math, utf8, bit32: no function allocates or loops beyond its arguments.
            _ => {}
        }
        lib.set_readonly(true);
        base.raw_set(*name, lib).map_err(e)?;
    }
    base.raw_set("pcall", make_pcall(lua, st).map_err(e)?)
        .map_err(e)?;

    for (name, entry) in ctx.entries() {
        let value = match entry {
            Entry::Member(member) => member_value(lua, st, name, member)?,
            Entry::Namespace(members) => {
                let ns = lua.create_table().map_err(e)?;
                for (member_name, member) in members {
                    let full = format!("{name}.{member_name}");
                    let v = member_value(lua, st, &full, member)?;
                    ns.raw_set(member_name.as_str(), v).map_err(e)?;
                }
                ns.set_readonly(true);
                Value::Table(ns)
            }
        };
        base.raw_set(name.as_str(), value).map_err(e)?;
    }
    base.set_readonly(true);

    let meta = lua.create_table().map_err(e)?;
    meta.raw_set("__index", base).map_err(e)?;
    meta.raw_set("__metatable", "locked").map_err(e)?;
    meta.set_readonly(true);
    let env = lua.create_table().map_err(e)?;
    env.set_metatable(Some(meta)).map_err(e)?;
    Ok(env)
}

fn member_value(lua: &Lua, st: &Rc<RunState>, name: &str, member: &Member) -> ScriptResult<Value> {
    match member {
        Member::Value(v) => to_lua(lua, v, &st.limits, Root::Named(name)),
        Member::Function(f) => host_function(lua, st, name, Arc::clone(f))
            .map(Value::Function)
            .map_err(|err| lua_error(err, &st.limits)),
    }
}

/// Wraps a host callback. Any failure at the boundary (argument conversion, the callback's error,
/// a panic, result conversion) stops the evaluation: the script sees only `host function '<name>'
/// failed`, the caller gets the typed error.
fn host_function(
    lua: &Lua,
    st: &Rc<RunState>,
    name: &str,
    f: Arc<HostFn>,
) -> mlua::Result<Function> {
    let st = Rc::clone(st);
    let name = name.to_string();
    lua.create_function(move |lua, args: MultiValue| {
        st.check_time()?;
        let failed = || mlua::Error::runtime(format!("host function '{name}' failed"));
        let mut converted = Vec::with_capacity(args.len());
        // One byte budget for all arguments of this call.
        let mut budget = Budget::new(&st.limits);
        for (i, arg) in args.iter().enumerate() {
            let root = Root::Argument {
                function: &name,
                index: i + 1,
            };
            match to_script(arg, &st.limits, root, &mut budget) {
                Ok(v) => converted.push(v),
                Err(err) => {
                    st.trip(err);
                    return Err(failed());
                }
            }
        }
        let result = match catch_unwind(AssertUnwindSafe(|| f(&converted))) {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => {
                st.trip(ScriptError::Host {
                    function: name.clone(),
                    source: err.0,
                });
                return Err(failed());
            }
            Err(payload) => {
                st.trip(ScriptError::Panic {
                    context: format!("host function '{name}'"),
                    message: panic_message(payload.as_ref()),
                });
                return Err(failed());
            }
        };
        st.check_time()?;
        match to_lua(lua, &result, &st.limits, Root::CallbackResult(&name)) {
            Ok(v) => Ok(v),
            Err(err) => {
                st.trip(err);
                Err(failed())
            }
        }
    })
}

/// The sandbox's `pcall`: catches ordinary script errors like Luau's, but re-raises once the
/// evaluation is stopped and turns an allocation failure into the (sticky) memory limit, so neither
/// a limit nor a host failure can be swallowed. The error value is always the message string.
fn make_pcall(lua: &Lua, st: &Rc<RunState>) -> mlua::Result<Function> {
    let st = Rc::clone(st);
    lua.create_function(move |lua, mut args: MultiValue| {
        st.ensure_running()?;
        let func = match args.pop_front() {
            Some(Value::Function(f)) => f,
            other => {
                let shown = other.as_ref().map_or("no", Value::type_name);
                let mut out = MultiValue::new();
                out.push_back(Value::Boolean(false));
                out.push_back(Value::String(
                    lua.create_string(format!("attempt to call a {shown} value"))?,
                ));
                return Ok(out);
            }
        };
        let _guard = st.enter("pcall")?;
        match st.relay(func.call::<MultiValue>(args)) {
            Ok(mut values) => {
                values.push_front(Value::Boolean(true));
                Ok(values)
            }
            Err(err) => {
                if st.tripped.get() {
                    return Err(err);
                }
                let mut out = MultiValue::new();
                out.push_back(Value::Boolean(false));
                out.push_back(Value::String(lua.create_string(error_message(&err))?));
                Ok(out)
            }
        }
    })
}

fn wrap_string_library(lua: &Lua, st: &Rc<RunState>, lib: &Table) -> mlua::Result<()> {
    for name in ["find", "match", "gmatch", "gsub"] {
        let original: Function = lib.raw_get(name)?;
        let st = Rc::clone(st);
        let wrapped = lua.create_function(move |_, args: MultiValue| {
            st.check_time()?;
            if let Some(len) = args.front().and_then(string_len)
                && len > PATTERN_SUBJECT_MAX_BYTES
            {
                return Err(mlua::Error::runtime(format!(
                    "string.{name}: subject is {len} bytes; the limit is {PATTERN_SUBJECT_MAX_BYTES} bytes"
                )));
            }
            if let Some(len) = args.get(1).and_then(string_len)
                && len > PATTERN_MAX_BYTES
            {
                return Err(mlua::Error::runtime(format!(
                    "string.{name}: pattern is {len} bytes; the limit is {PATTERN_MAX_BYTES} bytes"
                )));
            }
            // `gsub` may call a replacement function, which re-enters Luau through this frame.
            let _guard = st.enter(&format!("string.{name}"))?;
            let result = st.relay(original.call::<MultiValue>(args));
            st.check_time()?;
            result
        })?;
        lib.raw_set(name, wrapped)?;
    }

    let original_format: Function = lib.raw_get("format")?;
    let format = lua.create_function(move |lua, args: MultiValue| {
        // `%s` formats reference values with their heap address; show the type name instead.
        let mut mapped = MultiValue::new();
        for (i, arg) in args.into_iter().enumerate() {
            let keep = i == 0
                || matches!(
                    arg,
                    Value::Nil
                        | Value::Boolean(_)
                        | Value::Integer(_)
                        | Value::Number(_)
                        | Value::String(_)
                        | Value::Vector(_)
                );
            if keep {
                mapped.push_back(arg);
            } else {
                mapped.push_back(Value::String(lua.create_string(reference_type_name(&arg))?));
            }
        }
        original_format.call::<MultiValue>(mapped)
    })?;
    lib.raw_set("format", format)?;

    let original_rep: Function = lib.raw_get("rep")?;
    let st_rep = Rc::clone(st);
    // Luau 0.740 `str_rep` fills the result by doubling memcpy (`lstrlib.cpp:85-127`): its time is
    // linear in the result size and a zero-length string returns at once whatever the count, so the
    // memory charge bounds it; no per-repetition step charge is needed.
    let rep = lua.create_function(move |lua, args: MultiValue| {
        st_rep.check_time()?;
        let len = args.front().and_then(string_len);
        let count = args.get(1).and_then(integer_arg);
        if let (Some(len), Some(count)) = (len, count)
            && count > 0
        {
            // `None` (a count beyond usize or an overflowing product) trips the memory limit.
            let total = usize::try_from(count).ok().and_then(|c| c.checked_mul(len));
            st_rep.charge_memory(lua, total)?;
        }
        st_rep.relay(original_rep.call::<MultiValue>(args))
    })?;
    lib.raw_set("rep", rep)?;
    Ok(())
}

fn wrap_table_library(lua: &Lua, st: &Rc<RunState>, lib: &Table) -> mlua::Result<()> {
    let original_concat: Function = lib.raw_get("concat")?;
    let st_concat = Rc::clone(st);
    let concat = lua.create_function(move |lua, args: MultiValue| {
        st_concat.check_time()?;
        if let Some(Value::Table(t)) = args.front() {
            // An absent separator is the empty string; a non-string one makes the original reject
            // the call, so charging nothing for it is exact.
            let sep = match args.get(1) {
                None | Some(Value::Nil) => 0,
                Some(v) => string_len(v).unwrap_or(0),
            };
            let first = match args.get(2) {
                None | Some(Value::Nil) => Some(1),
                Some(v) => integer_arg(v),
            };
            let last = match args.get(3) {
                // A length beyond i64 cannot exist; `None` here (like a non-integer bound) leaves
                // the call to the original, which rejects bad bounds itself.
                None | Some(Value::Nil) => i64::try_from(t.raw_len()).ok(),
                Some(v) => integer_arg(v),
            };
            if let (Some(first), Some(last)) = (first, last) {
                let mut total: Option<usize> = Some(0);
                let mut i = first;
                // Stops at the first entry `table.concat` itself would reject, so the loop is
                // bounded by the table's size, not by the requested range.
                while i <= last {
                    st_concat.charge(1)?;
                    let item_len = match t.raw_get::<Value>(i)? {
                        Value::String(s) => s.as_bytes().len(),
                        Value::Integer(_) | Value::Number(_) => NUMBER_TEXT_BYTES,
                        _ => break,
                    };
                    total = total
                        .and_then(|t| t.checked_add(item_len))
                        .and_then(|t| t.checked_add(sep));
                    i += 1;
                }
                st_concat.charge_memory(lua, total)?;
            }
        }
        st_concat.relay(original_concat.call::<MultiValue>(args))
    })?;
    lib.raw_set("concat", concat)?;

    let original_move: Function = lib.raw_get("move")?;
    let st_move = Rc::clone(st);
    let mv = lua.create_function(move |_, args: MultiValue| {
        st_move.check_time()?;
        if let (Some(f), Some(e)) = (
            args.get(1).and_then(integer_arg),
            args.get(2).and_then(integer_arg),
        ) && e >= f
        {
            // One step per element moved: `table.move` loops in C without interrupts.
            let n = e.saturating_sub(f).saturating_add(1);
            // n >= 1 here; a count beyond u64 cannot happen for i64 inputs but would trip anyway.
            st_move.charge(u64::try_from(n).unwrap_or(u64::MAX))?;
        }
        st_move.relay(original_move.call::<MultiValue>(args))
    })?;
    lib.raw_set("move", mv)?;

    let original_sort: Function = lib.raw_get("sort")?;
    let st_sort = Rc::clone(st);
    let sort = lua.create_function(move |_, args: MultiValue| {
        st_sort.check_time()?;
        if let Some(Value::Table(t)) = args.front() {
            // Without a comparator `table.sort` runs in C without interrupts: charge n*log2(n)
            // steps up front (a comparator's own calls are counted by the interrupt as well).
            // usize fits u64 on every supported target; saturating would only over-charge.
            let n = u64::try_from(t.raw_len()).unwrap_or(u64::MAX);
            let log2 = u64::from(u64::BITS - n.leading_zeros());
            st_sort.charge(n.saturating_mul(log2))?;
        }
        // A comparator re-enters Luau through this frame.
        let _guard = st_sort.enter("table.sort")?;
        let result = st_sort.relay(original_sort.call::<MultiValue>(args));
        st_sort.check_time()?;
        result
    })?;
    lib.raw_set("sort", sort)?;
    Ok(())
}

/// `tostring` without heap addresses: tables, functions and other reference values print as their
/// type name (`"table"`), scalars as Luau prints them.
fn wrap_tostring(lua: &Lua, original: Function) -> mlua::Result<Function> {
    lua.create_function(move |lua, args: MultiValue| match args.front() {
        Some(
            Value::Nil
            | Value::Boolean(_)
            | Value::Integer(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Vector(_),
        )
        | None => original.call::<MultiValue>(args),
        Some(other) => {
            let mut out = MultiValue::new();
            out.push_back(Value::String(
                lua.create_string(reference_type_name(other))?,
            ));
            Ok(out)
        }
    })
}

/// Type name shown instead of an address. Light userdata and mlua errors are userdata to scripts.
fn reference_type_name(v: &Value) -> &'static str {
    match v {
        Value::Table(_) => "table",
        Value::Function(_) => "function",
        Value::Thread(_) => "thread",
        Value::Buffer(_) => "buffer",
        Value::UserData(_) | Value::LightUserData(_) | Value::Error(_) | Value::Other(..) => {
            "userdata"
        }
        Value::Nil
        | Value::Boolean(_)
        | Value::Integer(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Vector(_) => v.type_name(),
    }
}

fn wrap_error(lua: &Lua, original: Function, globals: &Table) -> mlua::Result<Function> {
    let typeof_fn: Function = globals.raw_get("typeof")?;
    // The helper only uses its two arguments; give it an empty, frozen environment anyway.
    let env = lua.create_table()?;
    env.set_readonly(true);
    lua.load(ERROR_WRAPPER)
        .set_name(HELPER_CHUNK_NAME)
        .set_mode(ChunkMode::Text)
        .set_environment(env)
        .into_function()?
        .call::<Function>((original, typeof_fn))
}

/// Byte length of a string argument; numbers (which Luau coerces) are short. `None` for other types,
/// which the original library function rejects itself.
fn string_len(v: &Value) -> Option<usize> {
    match v {
        Value::String(s) => Some(s.as_bytes().len()),
        Value::Integer(_) | Value::Number(_) => Some(NUMBER_TEXT_BYTES),
        _ => None,
    }
}

/// An integer argument the way `luaL_checkinteger` reads it (truncating a double).
fn integer_arg(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Number(f) if f.is_finite() => Some(f.trunc() as i64),
        _ => None,
    }
}

fn load_error(e: mlua::Error, limits: &ScriptLimits) -> ScriptError {
    match e {
        mlua::Error::SyntaxError { message, .. } => {
            let (line, message) = split_position(&message);
            ScriptError::Syntax { line, message }
        }
        other => lua_error(other, limits),
    }
}

fn call_error(e: mlua::Error, limits: &ScriptLimits) -> ScriptError {
    if is_memory_error(&e) {
        return limits.exceeded(LimitKind::Memory);
    }
    match e {
        mlua::Error::RuntimeError(text) => {
            let (message, traceback) = split_traceback(&text);
            ScriptError::Runtime { message, traceback }
        }
        mlua::Error::CallbackError { traceback, cause } => match root_cause(&cause) {
            mlua::Error::RuntimeError(text) => ScriptError::Runtime {
                message: split_traceback(text).0,
                traceback: Some(traceback),
            },
            other => ScriptError::engine(other),
        },
        other => ScriptError::engine(other),
    }
}

fn root_cause(e: &mlua::Error) -> &mlua::Error {
    match e {
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            root_cause(cause)
        }
        other => other,
    }
}

/// The message a script's `pcall` returns for a caught error.
fn error_message(e: &mlua::Error) -> String {
    match root_cause(e) {
        mlua::Error::RuntimeError(text) => split_traceback(text).0,
        other => other.to_string(),
    }
}

/// mlua appends `\nstack traceback:\n...` to string errors; split it off (the last occurrence, so a
/// script message containing the phrase stays intact).
fn split_traceback(text: &str) -> (String, Option<String>) {
    match text.rfind("\nstack traceback:\n") {
        Some(at) => (text[..at].to_string(), Some(text[at + 1..].to_string())),
        None => (text.to_string(), None),
    }
}

/// `script:3: message` -> `(Some(3), "message")`; anything else is kept whole with no line.
fn split_position(message: &str) -> (Option<u32>, String) {
    if let Some(rest) = message.strip_prefix(CHUNK_PREFIX)
        && let Some((line, text)) = rest.split_once(": ")
        && let Ok(line) = line.parse::<u32>()
    {
        return (Some(line), text.to_string());
    }
    (None, message.to_string())
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::BUILTIN_GLOBALS;

    #[test]
    fn builtin_list_matches_environment() {
        let mut env: Vec<&str> = BASE_FUNCTIONS.iter().chain(LIBRARIES).copied().collect();
        env.push("pcall");
        env.sort_unstable();
        let mut declared: Vec<&str> = BUILTIN_GLOBALS.to_vec();
        declared.sort_unstable();
        assert_eq!(env, declared);
    }

    /// Second layer under source validation: even real Luau bytecode handed straight to the loader
    /// is refused because chunks load in text mode.
    #[test]
    fn text_mode_refuses_real_bytecode() {
        let bytecode = mlua::chunk::Compiler::new().compile("return 1").unwrap();
        let lua = Lua::new_with(StdLib::NONE, LuaOptions::default()).unwrap();
        let err = lua
            .load(&bytecode[..])
            .set_name(CHUNK_NAME)
            .set_mode(ChunkMode::Text)
            .into_function()
            .unwrap_err();
        match load_error(err, &ScriptLimits::default()) {
            ScriptError::Syntax { line, message } => {
                assert_eq!(line, None);
                assert_eq!(message, "attempt to load a binary chunk (mode is 't')");
            }
            other => panic!("expected a syntax error, got {other:?}"),
        }
    }

    #[test]
    fn traceback_split_uses_last_marker() {
        let (msg, tb) =
            split_traceback("script:1: a\nstack traceback:\nfake\nstack traceback:\nreal");
        assert_eq!(msg, "script:1: a\nstack traceback:\nfake");
        assert_eq!(tb.as_deref(), Some("stack traceback:\nreal"));
        assert_eq!(split_traceback("plain"), ("plain".to_string(), None));
    }

    #[test]
    fn position_split() {
        assert_eq!(
            split_position("script:12: Expected 'end'"),
            (Some(12), "Expected 'end'".to_string())
        );
        assert_eq!(
            split_position("no position"),
            (None, "no position".to_string())
        );
    }
}
