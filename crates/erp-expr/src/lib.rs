//! Expressions are untrusted input. The evaluator has no path to the host beyond the names an
//! [`EvalContext`] binds, and every parse and evaluation runs under [`EvalLimits`]: source size,
//! nesting depth of the tree, of evaluation and of produced values, evaluation steps, and memory
//! charged in bytes of deep size before any value is built. Exceeding a limit is an
//! [`ExprError::LimitExceeded`]; nothing panics or aborts. Integers are 64-bit and checked.
//!
//! Layer L1: no I/O, no threads, no system clock (the host injects one), builds for
//! `wasm32-wasip2`.
//!
//! Host extension points ([`ExprObject`], [`ExprCallable`], the clock and the `ref` resolver) are
//! `Send + Sync`, so a context can be shared by worker threads.
//!
//! ```
//! use erp_expr::{EvalContext, ExprValue, eval};
//!
//! let ctx = EvalContext::standard().with_var("qty", ExprValue::Int(3));
//! assert_eq!(eval("qty * 2 if qty else 0", &ctx).unwrap(), ExprValue::Int(6));
//! ```

mod args;
mod ast;
mod builtins;
mod context;
mod datetime;
mod error;
mod eval;
mod format;
mod lexer;
mod limits;
mod methods;
mod modifier;
mod num;
mod ops;
mod parser;
pub mod proxy;
mod value;
pub mod wire;

pub use ast::{BinaryOp, BoolOp, CompareOp, Comprehension, Expr, Literal, SubscriptKind, UnaryOp};
pub use context::{Clock, EvalContext, RefResolver};
pub use datetime::{RelativeDelta, TimeDelta};
pub use error::{ExprError, ExprResult};
pub use eval::{eval, eval_ast};
pub use limits::{BYTES_PER_STEP, Budget, EvalLimits};
pub use modifier::combine_python_attribute;
pub use parser::{SourceParsePreflight, parse, parse_with_limits, source_preflight};
pub use proxy::{
    EnvProxy, GroupCheckResolver, HOST_SLICE, HostHooks, MAX_SEARCH_RESULTS, ModelProxy,
    RecordProxy, RecordResolver, RecordsetProxy, SearchQuery, SearchResolver,
    UserGroupCheckResolver, XidModelResolver,
};
pub use value::{ExprCallable, ExprObject, ExprValue, Kwargs, Map, Meta, REPR_SHORT_BYTES, Seq};
