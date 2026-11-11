//! What a host injects into one evaluation: values, callbacks, and the globals it wants back.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::{HostError, ScriptError, ScriptResult};
use crate::value::ScriptValue;

/// A host callback invocable from Luau. It receives converted arguments and returns a value or a
/// [`HostError`]. The script never sees the error itself (it gets a fixed message and the evaluation
/// fails); the caller gets [`ScriptError::Host`] with the error as `source`.
pub type HostFn = dyn Fn(&[ScriptValue]) -> Result<ScriptValue, HostError> + Send + Sync;

/// Global names provided by the sandbox itself; a context cannot shadow them.
pub(crate) const BUILTIN_GLOBALS: &[&str] = &[
    "assert", "error", "ipairs", "next", "pairs", "pcall", "rawequal", "rawget", "rawlen",
    "rawset", "select", "tonumber", "tostring", "type", "typeof", "unpack", "math", "string",
    "table", "utf8", "bit32",
];

/// Names the sandbox deliberately does not provide; a context cannot reintroduce them under the same
/// name either (a host that wants logging injects `log` or a namespaced function).
const FORBIDDEN_GLOBALS: &[&str] = &[
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
    "_ENV",
    "_VERSION",
];

const LUAU_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "continue",
];

#[derive(Clone)]
pub(crate) enum Member {
    Value(ScriptValue),
    Function(Arc<HostFn>),
}

#[derive(Clone)]
pub(crate) enum Entry {
    Member(Member),
    /// A read-only table of members: `env.search`, `env.ref`.
    Namespace(IndexMap<String, Member>),
}

/// Values and callbacks injected into an evaluation, and the globals read back after it.
///
/// Names are validated when added: a plain identifier (`record`) or one namespace level
/// (`env.search`); not a Luau keyword, not a sandbox builtin, not a deliberately absent global, not
/// used twice. Injected names live in a read-only base environment: a script may shadow one with
/// its own global, never modify the injected value's binding for other code.
#[derive(Clone, Default)]
pub struct ScriptContext {
    entries: IndexMap<String, Entry>,
    captures: Vec<String>,
}

impl ScriptContext {
    /// An empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Injects a value under `name` (`record` or `env.uid`).
    pub fn set_value(
        &mut self,
        name: &str,
        value: impl Into<ScriptValue>,
    ) -> ScriptResult<&mut Self> {
        self.insert(name, Member::Value(value.into()))?;
        Ok(self)
    }

    /// Registers a host callback under `name` (`log` or `env.search`).
    pub fn set_fn<F>(&mut self, name: &str, f: F) -> ScriptResult<&mut Self>
    where
        F: Fn(&[ScriptValue]) -> Result<ScriptValue, HostError> + Send + Sync + 'static,
    {
        self.insert(name, Member::Function(Arc::new(f)))?;
        Ok(self)
    }

    /// Only what the script itself
    /// assigned is returned; an unassigned global comes back as `Nil`.
    pub fn capture(&mut self, name: &str) -> ScriptResult<&mut Self> {
        validate_ident(name, name)?;
        if self.entries.contains_key(name) {
            return Err(invalid(
                name,
                "already injected; a captured global must be script-owned",
            ));
        }
        if self.captures.iter().any(|c| c == name) {
            return Err(invalid(name, "already captured"));
        }
        self.captures.push(name.to_string());
        Ok(self)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.entries.iter()
    }

    pub(crate) fn captures(&self) -> &[String] {
        &self.captures
    }

    fn insert(&mut self, name: &str, member: Member) -> ScriptResult<()> {
        let (head, tail) = match name.split_once('.') {
            Some((head, tail)) => (head, Some(tail)),
            None => (name, None),
        };
        validate_ident(name, head)?;
        if self.captures.iter().any(|c| c == head) {
            return Err(invalid(name, "already captured as a script global"));
        }
        match tail {
            None => {
                if self.entries.contains_key(head) {
                    return Err(invalid(name, "already defined"));
                }
                self.entries.insert(head.to_string(), Entry::Member(member));
            }
            Some(tail) => {
                if tail.contains('.') {
                    return Err(invalid(name, "only one namespace level is supported"));
                }
                validate_member(name, tail)?;
                let slot = self
                    .entries
                    .entry(head.to_string())
                    .or_insert_with(|| Entry::Namespace(IndexMap::new()));
                match slot {
                    Entry::Namespace(members) => {
                        if members.contains_key(tail) {
                            return Err(invalid(name, "already defined"));
                        }
                        members.insert(tail.to_string(), member);
                    }
                    Entry::Member(_) => {
                        return Err(invalid(name, "the namespace is already defined as a value"));
                    }
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ScriptContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<String> = Vec::new();
        for (name, entry) in &self.entries {
            match entry {
                Entry::Member(_) => names.push(name.clone()),
                Entry::Namespace(members) => {
                    names.extend(members.keys().map(|m| format!("{name}.{m}")));
                }
            }
        }
        f.debug_struct("ScriptContext")
            .field("names", &names)
            .field("captures", &self.captures)
            .finish()
    }
}

fn invalid(name: &str, reason: &'static str) -> ScriptError {
    ScriptError::InvalidName {
        name: name.to_string(),
        reason,
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        Some(_) | None => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A namespace member is a table key, so builtin names are fine (`env.type`), keywords are not
/// (they could not be written as `env.end`).
fn validate_member(full: &str, member: &str) -> ScriptResult<()> {
    if !is_ident(member) {
        return Err(invalid(full, "not an identifier ([A-Za-z_][A-Za-z0-9_]*)"));
    }
    if LUAU_KEYWORDS.contains(&member) {
        return Err(invalid(full, "a Luau keyword"));
    }
    Ok(())
}

fn validate_ident(full: &str, ident: &str) -> ScriptResult<()> {
    validate_member(full, ident)?;
    if BUILTIN_GLOBALS.contains(&ident) {
        return Err(invalid(full, "a sandbox builtin"));
    }
    if FORBIDDEN_GLOBALS.contains(&ident) {
        return Err(invalid(full, "deliberately absent from the sandbox"));
    }
    Ok(())
}
