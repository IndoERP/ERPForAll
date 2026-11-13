//! Luau sandbox for ERPForAll: `code` server actions, automated actions, and the Luau hooks and
//! migrations of data-only addons (§7, §10).
//!
//! Scripts are untrusted. Each evaluation gets a fresh Luau VM with:
//! - an environment built from an explicit allowlist (`math`, `string`, `table`, `utf8`, `bit32`,
//!   basic functions, the sandbox's own `pcall`); no `require`, `load`/`loadstring`,
//!   `getfenv`/`setfenv`, `newproxy`, `collectgarbage`, `print`, `debug`, `os`, `io`, metatables;
//! - text-only loading: bytecode, NUL, control bytes and invalid UTF-8 are rejected;
//! - limits on source size, steps, wall-clock time and memory ([`ScriptLimits`]) that trip a sticky
//!   flag a script can neither catch nor forge;
//! - bounded C-side work: capped pattern functions, memory-charged `string.rep`/`table.concat`;
//! - exact, bounded value conversion ([`ScriptValue`]).
//!
//! The host integration (transaction, caller's rights, async) is;
//! the server action runner itself is.

mod context;
mod convert;
mod error;
mod limits;
mod sandbox;
mod value;

pub use context::{HostFn, ScriptContext};
pub use error::{BoxedHostError, HostError, LimitKind, ScriptError, ScriptResult, SourceIssue};
pub use limits::{NATIVE_NESTING_MAX, PATTERN_MAX_BYTES, PATTERN_SUBJECT_MAX_BYTES, ScriptLimits};
pub use sandbox::{Evaluation, LuauSandbox, ScriptCancellation};
pub use value::{LUAU_EXACT_INT_MAX, ScriptValue};
