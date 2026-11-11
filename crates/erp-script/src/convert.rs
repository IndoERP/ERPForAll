//! Exact conversions between Luau values and [`ScriptValue`].
//!
//! Both directions are bounded by the depth and value-count limits, read tables raw (no `__index`,
//! no `__pairs`), detect cycles, and fail with [`ScriptError::Type`] instead of losing information.
//! The rules are listed on [`ScriptValue`].

use std::ffi::c_void;

use indexmap::IndexMap;
use mlua::{Lua, LuaString, Table, Value};

use crate::error::{LimitKind, ScriptError, ScriptResult};
use crate::limits::ScriptLimits;
use crate::value::{LUAU_EXACT_INT_MAX, ScriptValue};

/// One step of the path to a value. Holds handles, not copies: the path is rendered only when an
/// error is reported, so converting a value never duplicates its keys.
enum Seg<'a> {
    LuaKey(LuaString),
    HostKey(&'a str),
    Index(usize),
}

/// What is being converted; the start of every error path.
#[derive(Clone, Copy)]
pub(crate) enum Root<'a> {
    /// The script's return value: `$`.
    Result,
    /// A captured global or an injected host value: its name.
    Named(&'a str),
    /// Argument `index` (1-based) of a host callback: `env.search argument 2`.
    Argument { function: &'a str, index: usize },
    /// The result of a host callback: `env.search result`.
    CallbackResult(&'a str),
}

/// Where in a value the converter is; rendered only for error messages.
struct Path<'a> {
    root: Root<'a>,
    segs: Vec<Seg<'a>>,
}

impl Path<'_> {
    fn render(&self) -> String {
        let mut out = match self.root {
            Root::Result => "$".to_string(),
            Root::Named(name) => name.to_string(),
            Root::Argument { function, index } => format!("{function} argument {index}"),
            Root::CallbackResult(function) => format!("{function} result"),
        };
        for seg in &self.segs {
            out.push_str(&match seg {
                Seg::LuaKey(k) => render_key(&k.to_string_lossy()),
                Seg::HostKey(k) => render_key(k),
                Seg::Index(i) => format!("[{i}]"),
            });
        }
        out
    }
}

/// Keys longer than this are cut in error paths, so a message stays small whatever the script built.
const KEY_SHOWN_BYTES: usize = 64;

fn render_key(k: &str) -> String {
    if is_plain_key(k) && k.len() <= KEY_SHOWN_BYTES {
        return format!(".{k}");
    }
    let mut end = k.len().min(KEY_SHOWN_BYTES);
    while !k.is_char_boundary(end) {
        end -= 1;
    }
    if end < k.len() {
        format!("[{:?}...]", &k[..end])
    } else {
        format!("[{k:?}]")
    }
}

fn is_plain_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Host bytes a conversion may produce (string values, byte strings and keys). One budget can span
/// several conversions (a script result and its captured globals). Capped at the memory limit: a
/// Luau string repeated many times is stored once in the VM, but every host copy is separate.
pub(crate) struct Budget {
    left: usize,
}

impl Budget {
    pub(crate) fn new(limits: &ScriptLimits) -> Self {
        Budget {
            left: limits.memory_bytes(),
        }
    }
}

struct ToScript<'a, 'b> {
    limits: &'a ScriptLimits,
    budget: &'b mut Budget,
    nodes: usize,
    /// Tables on the current path, for cycle detection. Shared, acyclic sub-tables are allowed and
    /// counted on every visit (the value-count and byte limits bound the blow-up).
    open: Vec<*const c_void>,
    path: Path<'a>,
}

/// Converts a Luau value to a [`ScriptValue`], charging `budget` for the host bytes it builds.
pub(crate) fn to_script<'a>(
    value: &Value,
    limits: &'a ScriptLimits,
    root: Root<'a>,
    budget: &mut Budget,
) -> ScriptResult<ScriptValue> {
    let mut conv = ToScript {
        limits,
        budget,
        nodes: 0,
        open: Vec::new(),
        path: Path {
            root,
            segs: Vec::new(),
        },
    };
    conv.value(value)
}

impl ToScript<'_, '_> {
    fn err(&self, message: impl Into<String>) -> ScriptError {
        ScriptError::type_error(self.path.render(), message)
    }

    fn count(&mut self, n: usize) -> ScriptResult<()> {
        self.nodes = self.nodes.saturating_add(n);
        if self.nodes > self.limits.nodes() {
            return Err(self.limits.exceeded(LimitKind::Nodes));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> ScriptResult<()> {
        match self.budget.left.checked_sub(bytes) {
            Some(left) => {
                self.budget.left = left;
                Ok(())
            }
            None => Err(self.limits.exceeded(LimitKind::ConvertedBytes)),
        }
    }

    fn value(&mut self, value: &Value) -> ScriptResult<ScriptValue> {
        self.count(1)?;
        match value {
            Value::Nil => Ok(ScriptValue::Nil),
            Value::Boolean(b) => Ok(ScriptValue::Bool(*b)),
            // mlua yields `Integer` exactly when the double is integral and fits i64.
            Value::Integer(i) => Ok(ScriptValue::Int(*i)),
            Value::Number(f) if f.is_finite() => Ok(ScriptValue::Float(*f)),
            Value::Number(f) => Err(self.err(format!("number {f} has no host representation"))),
            Value::String(s) => {
                self.charge(s.as_bytes().len())?;
                match s.to_str() {
                    Ok(text) => Ok(ScriptValue::String(text.to_string())),
                    // Not UTF-8: kept byte for byte.
                    Err(_) => Ok(ScriptValue::Bytes(s.as_bytes().to_vec())),
                }
            }
            Value::Table(t) => self.table(t),
            other => Err(self.err(format!(
                "a {} cannot be passed to the host",
                other.type_name()
            ))),
        }
    }

    fn table(&mut self, t: &Table) -> ScriptResult<ScriptValue> {
        if self.open.len() >= self.limits.depth() {
            return Err(self.limits.exceeded(LimitKind::Depth));
        }
        let ptr = t.to_pointer();
        if self.open.contains(&ptr) {
            return Err(self.err("cyclic table reference"));
        }
        if t.metatable().is_some() {
            return Err(self.err("a table with a metatable cannot be passed to the host"));
        }

        let mut pairs: Vec<(Value, Value)> = Vec::new();
        for pair in t.pairs::<Value, Value>() {
            let (k, v) = pair.map_err(ScriptError::engine)?;
            // Count entries while collecting so a huge table stops early.
            self.count(1)?;
            pairs.push((k, v));
        }

        self.open.push(ptr);
        let result = self.classify(pairs);
        self.open.pop();
        result
    }

    fn classify(&mut self, pairs: Vec<(Value, Value)>) -> ScriptResult<ScriptValue> {
        if pairs.is_empty() {
            return Ok(ScriptValue::Dict(IndexMap::new()));
        }
        let any_string = pairs.iter().any(|(k, _)| matches!(k, Value::String(_)));
        if any_string {
            if let Some((k, _)) = pairs.iter().find(|(k, _)| !matches!(k, Value::String(_))) {
                return Err(self.mixed(k));
            }
            let mut entries: Vec<(LuaString, Value)> = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                if let Value::String(key) = k {
                    if key.to_str().is_err() {
                        return Err(self.err("table key is not valid UTF-8"));
                    }
                    entries.push((key, v));
                }
            }
            // The key borrows from the element, which `sort_by_key` cannot express.
            #[allow(clippy::unnecessary_sort_by)]
            entries.sort_by(|a, b| a.0.as_bytes().cmp(&b.0.as_bytes()));
            let mut dict = IndexMap::with_capacity(entries.len());
            for (key, v) in entries {
                self.charge(key.as_bytes().len())?;
                let owned = key.to_str().map_err(ScriptError::engine)?.to_string();
                self.path.segs.push(Seg::LuaKey(key));
                let converted = self.value(&v);
                self.path.segs.pop();
                dict.insert(owned, converted?);
            }
            return Ok(ScriptValue::Dict(dict));
        }

        let n = pairs.len();
        let mut slots: Vec<Option<Value>> = vec![None; n];
        for (k, v) in pairs {
            match k {
                Value::Integer(i) if i >= 1 && usize::try_from(i).is_ok_and(|i| i <= n) => {
                    // i in 1..=n, so the index is in bounds; keys are unique in a table.
                    let idx = usize::try_from(i - 1).map_err(ScriptError::engine)?;
                    slots[idx] = Some(v);
                }
                Value::Integer(i) => {
                    return Err(self.err(format!(
                        "array has a hole: index {i} in a table of {n} entries (keys must be 1..={n})"
                    )));
                }
                other => {
                    return Err(self.err(format!(
                        "unsupported table key of type {}",
                        key_desc(&other)
                    )));
                }
            }
        }
        let mut list = Vec::with_capacity(n);
        for (idx, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(v) => {
                    self.path.segs.push(Seg::Index(idx + 1));
                    let converted = self.value(&v);
                    self.path.segs.pop();
                    list.push(converted?);
                }
                None => {
                    return Err(self.err(format!("array has a hole at index {}", idx + 1)));
                }
            }
        }
        Ok(ScriptValue::List(list))
    }

    fn mixed(&self, key: &Value) -> ScriptError {
        self.err(format!(
            "table mixes string keys with a key of type {}",
            key_desc(key)
        ))
    }
}

fn key_desc(key: &Value) -> String {
    match key {
        Value::Integer(i) => format!("number ({i})"),
        Value::Number(f) => format!("number ({f})"),
        Value::Boolean(b) => format!("boolean ({b})"),
        Value::String(_) => "string".to_string(),
        other => other.type_name().to_string(),
    }
}

/// Converts a [`ScriptValue`] to a Luau value. Bounded by the depth limit; integers beyond ±2^53
/// are rejected because a Luau number cannot hold them exactly.
pub(crate) fn to_lua<'a>(
    lua: &Lua,
    value: &'a ScriptValue,
    limits: &ScriptLimits,
    root: Root<'a>,
) -> ScriptResult<Value> {
    let mut path = Path {
        root,
        segs: Vec::new(),
    };
    to_lua_inner(lua, value, limits, &mut path, 0)
}

fn to_lua_inner<'a>(
    lua: &Lua,
    value: &'a ScriptValue,
    limits: &ScriptLimits,
    path: &mut Path<'a>,
    depth: usize,
) -> ScriptResult<Value> {
    match value {
        ScriptValue::Nil => Ok(Value::Nil),
        ScriptValue::Bool(b) => Ok(Value::Boolean(*b)),
        ScriptValue::Int(i) => {
            if i.unsigned_abs() > LUAU_EXACT_INT_MAX.unsigned_abs() {
                return Err(ScriptError::type_error(
                    path.render(),
                    format!("integer {i} cannot be represented exactly in Luau (limit is ±2^53)"),
                ));
            }
            Ok(Value::Integer(*i))
        }
        ScriptValue::Float(f) => Ok(Value::Number(*f)),
        ScriptValue::String(s) => lua
            .create_string(s)
            .map(Value::String)
            .map_err(|e| lua_error(e, limits)),
        ScriptValue::Bytes(b) => lua
            .create_string(b)
            .map(Value::String)
            .map_err(|e| lua_error(e, limits)),
        ScriptValue::List(items) => {
            if depth >= limits.depth() {
                return Err(limits.exceeded(LimitKind::Depth));
            }
            let table = lua
                .create_table_with_capacity(items.len(), 0)
                .map_err(|e| lua_error(e, limits))?;
            for (idx, item) in items.iter().enumerate() {
                path.segs.push(Seg::Index(idx + 1));
                let converted = to_lua_inner(lua, item, limits, path, depth + 1);
                path.segs.pop();
                table
                    .raw_set(idx + 1, converted?)
                    .map_err(|e| lua_error(e, limits))?;
            }
            Ok(Value::Table(table))
        }
        ScriptValue::Dict(entries) => {
            if depth >= limits.depth() {
                return Err(limits.exceeded(LimitKind::Depth));
            }
            let table = lua
                .create_table_with_capacity(0, entries.len())
                .map_err(|e| lua_error(e, limits))?;
            for (k, v) in entries {
                path.segs.push(Seg::HostKey(k.as_str()));
                let converted = to_lua_inner(lua, v, limits, path, depth + 1);
                path.segs.pop();
                table
                    .raw_set(k.as_str(), converted?)
                    .map_err(|e| lua_error(e, limits))?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Maps an mlua error raised while building host values: an allocation failure is the memory limit,
/// anything else an engine error.
pub(crate) fn lua_error(e: mlua::Error, limits: &ScriptLimits) -> ScriptError {
    if is_memory_error(&e) {
        limits.exceeded(LimitKind::Memory)
    } else {
        ScriptError::engine(e)
    }
}

/// True when `e` is (or wraps) a Luau allocation failure. Decided on mlua's error variant, never on
/// a message.
pub(crate) fn is_memory_error(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            is_memory_error(cause)
        }
        // Every other variant is a different failure, not an allocation failure.
        _ => false,
    }
}
