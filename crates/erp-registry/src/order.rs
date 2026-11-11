//! Addon load order: `(depth, name)` over the dependency graph.

use erp_fields::{Error, Result};
use rustc_hash::FxHashMap;

use crate::decl::AddonDescriptor;

/// Sort addons by `(depth, name)`, where depth is 0 for an addon without dependencies and one more
/// than its deepest dependency otherwise. Missing dependencies, cycles and two different descriptors
/// with one name are errors.
pub fn load_order(addons: &[&'static AddonDescriptor]) -> Result<Vec<&'static AddonDescriptor>> {
    let mut by_name: FxHashMap<&str, &'static AddonDescriptor> = FxHashMap::default();
    for addon in addons {
        match by_name.get(addon.name) {
            Some(existing) if !std::ptr::eq(*existing, *addon) => {
                return Err(Error::programming(format!(
                    "two different descriptors for addon '{}'",
                    addon.name
                )));
            }
            _ => {
                by_name.insert(addon.name, addon);
            }
        }
    }
    let mut depth: FxHashMap<&str, usize> = FxHashMap::default();
    fn visit(
        name: &'static str,
        by_name: &FxHashMap<&str, &'static AddonDescriptor>,
        depth: &mut FxHashMap<&'static str, usize>,
        stack: &mut Vec<&'static str>,
    ) -> Result<usize> {
        if let Some(d) = depth.get(name) {
            return Ok(*d);
        }
        if stack.contains(&name) {
            return Err(Error::programming(format!(
                "addon dependency cycle: {} -> {name}",
                stack.join(" -> ")
            )));
        }
        let Some(addon) = by_name.get(name) else {
            return Err(Error::programming(format!(
                "addon '{}' depends on '{name}', which is not in the addon set",
                stack.last().copied().unwrap_or("?")
            )));
        };
        stack.push(name);
        let mut d = 0;
        for dep in addon.depends {
            d = d.max(visit(dep, by_name, depth, stack)? + 1);
        }
        stack.pop();
        depth.insert(name, d);
        Ok(d)
    }
    let mut stack = Vec::new();
    let mut ordered: Vec<&'static AddonDescriptor> = by_name.values().copied().collect();
    for addon in &ordered {
        visit(addon.name, &by_name, &mut depth, &mut stack)?;
    }
    ordered.sort_by_key(|a| (depth.get(a.name).copied().unwrap_or(0), a.name));
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    static BASE: AddonDescriptor = AddonDescriptor {
        name: "base",
        depends: &[],
        exact_only: false,
    };
    static B: AddonDescriptor = AddonDescriptor {
        name: "b",
        depends: &["base"],
        exact_only: false,
    };
    static A: AddonDescriptor = AddonDescriptor {
        name: "a",
        depends: &["b"],
        exact_only: false,
    };
    static C: AddonDescriptor = AddonDescriptor {
        name: "c",
        depends: &["base"],
        exact_only: false,
    };
    static MISSING: AddonDescriptor = AddonDescriptor {
        name: "m",
        depends: &["nope"],
        exact_only: false,
    };

    #[test]
    fn depth_then_name() {
        let order = load_order(&[&A, &C, &B, &BASE]).unwrap();
        let names: Vec<_> = order.iter().map(|a| a.name).collect();
        assert_eq!(names, ["base", "b", "c", "a"]);
    }

    #[test]
    fn missing_dependency_is_an_error() {
        let err = load_order(&[&BASE, &MISSING]).unwrap_err();
        assert!(err.message().contains("'nope'"), "{err}");
    }
}
