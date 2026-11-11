//! Argument binding for builtins and methods: positional and keyword arguments are matched against
//! a signature; unknown keywords, duplicates, missing and surplus arguments are `TypeError`s
//! (review round 2: "builtins reject unknown kwargs and accept the documented ones").

use crate::error::{ExprError, ExprResult};
use crate::value::{ExprValue, Kwargs};

/// A builtin signature.
pub(crate) struct Sig {
    /// Function name used in messages.
    pub name: &'static str,
    /// Parameter names in positional order.
    pub params: &'static [&'static str],
    /// The first `required` parameters must be given.
    pub required: usize,
    /// The first `pos_only` parameters cannot be passed by keyword.
    pub pos_only: usize,
    /// At most `max_pos` arguments may be passed positionally.
    pub max_pos: usize,
}

impl Sig {
    /// Binds `args`/`kwargs`; `result[i]` is the value of `params[i]`, `None` when absent.
    pub(crate) fn bind<'v>(
        &self,
        args: &'v [ExprValue],
        kwargs: &'v Kwargs,
    ) -> ExprResult<Vec<Option<&'v ExprValue>>> {
        if args.len() > self.max_pos {
            return Err(ExprError::type_error(if self.max_pos == 0 {
                format!(
                    "{}() takes no positional arguments ({} given)",
                    self.name,
                    args.len()
                )
            } else {
                format!(
                    "{}() takes at most {} argument{} ({} given)",
                    self.name,
                    self.max_pos,
                    if self.max_pos == 1 { "" } else { "s" },
                    args.len()
                )
            }));
        }
        let mut slots: Vec<Option<&ExprValue>> = vec![None; self.params.len()];
        for (slot, arg) in slots.iter_mut().zip(args) {
            *slot = Some(arg);
        }
        for (key, value) in kwargs {
            let index = self
                .params
                .iter()
                .position(|p| p == key)
                .filter(|&i| i >= self.pos_only)
                .ok_or_else(|| {
                    ExprError::type_error(format!(
                        "{}() got an unexpected keyword argument '{key}'",
                        self.name
                    ))
                })?;
            match slots.get_mut(index) {
                Some(slot @ None) => *slot = Some(value),
                _ => {
                    return Err(ExprError::type_error(format!(
                        "{}() got multiple values for argument '{key}'",
                        self.name
                    )));
                }
            }
        }
        for (param, slot) in self.params.iter().zip(&slots).take(self.required) {
            if slot.is_none() {
                return Err(ExprError::type_error(format!(
                    "{}() missing 1 required positional argument: '{param}'",
                    self.name
                )));
            }
        }
        Ok(slots)
    }
}

/// CPython names keyword parameters in quotes and positional-only ones by number.
fn param_label(param: &str) -> String {
    if param.chars().all(|c| c.is_ascii_digit()) {
        param.to_string()
    } else {
        format!("'{param}'")
    }
}

/// The value of an `int`/`bool` argument, a `TypeError` naming the parameter otherwise.
pub(crate) fn int_arg(fname: &str, param: &str, v: &ExprValue) -> ExprResult<i64> {
    v.as_int().ok_or_else(|| {
        ExprError::type_error(format!(
            "{fname}() argument {} must be int, not {}",
            param_label(param),
            v.type_name()
        ))
    })
}

pub(crate) fn str_arg<'v>(fname: &str, param: &str, v: &'v ExprValue) -> ExprResult<&'v str> {
    v.as_str().ok_or_else(|| {
        ExprError::type_error(format!(
            "{fname}() argument {} must be str, not {}",
            param_label(param),
            v.type_name()
        ))
    })
}

/// A required slot: `bind` has already checked presence for the first `required` parameters, so
/// absence here is a signature bug, reported as an error instead of a panic.
pub(crate) fn required<'v>(
    sig: &Sig,
    slots: &[Option<&'v ExprValue>],
    i: usize,
) -> ExprResult<&'v ExprValue> {
    slots.get(i).copied().flatten().ok_or_else(|| {
        ExprError::type_error(format!(
            "{}() missing 1 required positional argument: '{}'",
            sig.name,
            // Only reached through a signature bug; the name is for the message alone.
            sig.params.get(i).copied().unwrap_or("?")
        ))
    })
}

/// An optional slot.
pub(crate) fn optional<'v>(slots: &[Option<&'v ExprValue>], i: usize) -> Option<&'v ExprValue> {
    slots.get(i).copied().flatten()
}
