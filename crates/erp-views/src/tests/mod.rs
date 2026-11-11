//! In-crate tests for the engine internals (locator, XPath, spec application, parsing limits).
//! The public combination API is tested in `tests/combine.rs`.

mod filtered;
mod guard_capture;
mod guard_exprs;
mod hardening;
#[cfg(feature = "host-access")]
mod host_access;
mod locator;
mod positions;
mod regressions;
mod view_type_callbacks;
mod view_wire_registry;
mod work_limits;
mod xpath_grammar;

use crate::apply::apply_inheritance_specs;
use crate::arch::{Arch, Caps, NodeId};
use crate::error::{ViewError, ViewResult};
use crate::limits::{Budget, ViewLimits};
use crate::locate::locate;
use crate::parse::parse_arch;
use crate::xpath::XPath;

/// Parses with the default limits; test inputs are well-formed.
pub(crate) fn arch(xml: &str) -> Arch {
    parse_arch(xml, &ViewLimits::default(), 1).expect("test arch parses")
}

/// Applies `spec` to `base` like a combination does and returns the serialized result.
pub(crate) fn apply(base: &str, spec: &str) -> ViewResult<String> {
    apply_with(base, spec, &ViewLimits::default())
}

pub(crate) fn apply_with(base: &str, spec: &str, limits: &ViewLimits) -> ViewResult<String> {
    let mut source = arch(base);
    source.set_caps(Caps::combined(limits));
    let spec = arch(spec);
    apply_inheritance_specs(source, &spec, limits, &mut Budget::new(limits)).map(|a| a.to_xml())
}

/// Locates with the spec written as XML; the spec's root element is the spec node.
pub(crate) fn locate_xml(source: &Arch, spec: &str) -> ViewResult<Option<NodeId>> {
    let spec = arch(spec);
    let limits = ViewLimits::default();
    locate(
        source,
        &spec,
        spec.root_id(),
        &limits,
        &mut Budget::new(&limits),
    )
}

/// Element-child indices from the root to `id`: which node, not just which tag.
pub(crate) fn path_of(arch: &Arch, id: NodeId) -> Vec<usize> {
    let mut path = Vec::new();
    let mut cur = id;
    while let Some(parent) = arch.parent(cur) {
        let index = arch
            .element_children(parent)
            .position(|c| c == cur)
            .expect("child is listed under its parent");
        path.push(index);
        cur = parent;
    }
    path.reverse();
    path
}

/// All matches of `expr` as (path, opening tag) pairs, in document order.
pub(crate) fn select_all(source: &Arch, expr: &str) -> ViewResult<Vec<Vec<usize>>> {
    let limits = ViewLimits::default();
    let xpath = XPath::parse(expr, &limits)?;
    Ok(xpath
        .all(source, &mut Budget::new(&limits))?
        .into_iter()
        .map(|id| path_of(source, id))
        .collect())
}

pub(crate) fn xpath_error(expr: &str) -> ViewError {
    XPath::parse(expr, &ViewLimits::default()).expect_err("expression is rejected")
}

/// Runs `f` on a thread with a 256 KiB stack (an eighth of the default): inputs that used to recurse per nesting level must
/// fail cleanly here instead of overflowing.
pub(crate) fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(f)
        .expect("spawn small-stack thread")
        .join()
        .expect("small-stack thread finished without panicking or overflowing")
}
