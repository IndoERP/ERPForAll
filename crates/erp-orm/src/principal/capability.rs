//! Named capabilities (DN §2.1, R-S8): admin checks sit behind names such as
//! `principal.impersonate`, mapped to groups by the `ir.capability` table.
//!
//! A [`Capability`] is declared as a `static` by [`capability!`](crate::capability),
//! which also registers it in [`CAPABILITIES`]. Duplicate or malformed names fail
//! [`crate::OrmBuilder::build`], so a misdeclared capability never reaches a running server.

use super::error::PrincipalError;
use super::ids::truncate;

/// Longest capability name (bytes).
pub const MAX_CAPABILITY_LEN: usize = 64;

/// A named capability. Declare one with [`capability!`](crate::capability). The hidden
/// `__declare` the macro expands to is public, so a value can be built outside it;
/// [`registered_capability`] (what `has_capability` goes through) refuses any value that is not a
/// member of [`CAPABILITIES`].
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Capability {
    name: &'static str,
    declared_in: &'static str,
}

impl Capability {
    /// Used by [`capability!`](crate::capability) only.
    #[doc(hidden)]
    pub const fn __declare(name: &'static str, declared_in: &'static str) -> Self {
        Self { name, declared_in }
    }

    /// The name, e.g. `principal.impersonate`.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Module path of the declaration.
    pub fn declared_in(&self) -> &'static str {
        self.declared_in
    }
}

/// Every capability linked into the binary.
#[linkme::distributed_slice]
pub static CAPABILITIES: [Capability];

/// The registered capability `cap` stands for: `cap` itself when it is a member of
/// [`CAPABILITIES`] (pointer-equal, not merely equal by name), so a policy's `has_capability`
/// accepts only values `capability!` created. A value built through the hidden
/// [`Capability::__declare`] outside the macro, or a by-value copy of a registered one, is
/// refused.
///
/// ```
/// erp_orm::capability!(pub EXPORT_ALL = "my_addon.export_all_2");
/// let registered = erp_orm::registered_capability(&EXPORT_ALL)?;
/// assert_eq!(registered.name(), "my_addon.export_all_2");
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
///
/// # Errors
///
/// [`PrincipalError::UnregisteredCapability`] (kind `Programming`).
pub fn registered_capability(cap: &Capability) -> Result<&'static Capability, PrincipalError> {
    CAPABILITIES
        .iter()
        .find(|member| std::ptr::eq(*member, cap))
        .ok_or_else(|| PrincipalError::UnregisteredCapability {
            name: super::ids::truncate(cap.name),
            declared_in: cap.declared_in,
        })
}

/// Declare a capability: a `static` of type [`Capability`] registered in [`CAPABILITIES`].
///
/// Names are dot-separated parts of lowercase ASCII letters, digits and `_`, at most 64 bytes;
/// [`crate::OrmBuilder::build`] refuses malformed names and a name declared twice.
///
/// ```
/// erp_orm::capability!(pub EXPORT_ALL = "my_addon.export_all");
///
/// fn main() {
///     assert_eq!(EXPORT_ALL.name(), "my_addon.export_all");
///     assert!(erp_orm::CAPABILITIES.iter().any(|c| c.name() == "my_addon.export_all"));
/// }
/// ```
#[macro_export]
macro_rules! capability {
    ($(#[$meta:meta])* $vis:vis $ident:ident = $name:literal) => {
        $(#[$meta])*
        #[$crate::linkme::distributed_slice($crate::CAPABILITIES)]
        #[linkme(crate = $crate::linkme)]
        $vis static $ident: $crate::Capability =
            $crate::Capability::__declare($name, ::core::module_path!());
    };
}

/// Check names and uniqueness of `capabilities`.
///
/// # Errors
///
/// [`PrincipalError::InvalidCapabilityName`] or [`PrincipalError::DuplicateCapability`] for the
/// first offending declaration.
pub(crate) fn validate<'a>(
    capabilities: impl IntoIterator<Item = &'a Capability>,
) -> Result<(), PrincipalError> {
    let mut seen: Vec<&Capability> = Vec::new();
    for cap in capabilities {
        if !valid_name(cap.name) {
            return Err(PrincipalError::InvalidCapabilityName {
                name: truncate(cap.name),
                declared_in: cap.declared_in,
            });
        }
        if let Some(first) = seen.iter().find(|c| c.name == cap.name) {
            return Err(PrincipalError::DuplicateCapability {
                name: cap.name,
                first: first.declared_in,
                second: cap.declared_in,
            });
        }
        seen.push(cap);
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CAPABILITY_LEN
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_and_bad_names_are_refused() {
        let a = Capability::__declare("sale.approve", "addon_a");
        let b = Capability::__declare("sale.approve", "addon_b");
        let c = Capability::__declare("sale.other", "addon_c");
        let err = validate([&a, &c, &b]).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "capability 'sale.approve' is declared twice (in addon_a and in addon_b)"
        );
        validate([&a, &c]).unwrap();
        validate([]).unwrap();
        let long = "a".repeat(MAX_CAPABILITY_LEN + 1).leak();
        for bad in [
            "", ".", "a..b", "a.", ".a", "A.b", "a-b", "ü.b", "a b", "a\n", &*long,
        ] {
            let cap = Capability::__declare(bad, "addon_x");
            let err = validate([&cap]).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "invalid capability name '{}' (declared in addon_x): use lowercase ASCII \
                     letters, digits and '_' in dot-separated parts, at most 64 bytes",
                    truncate(bad)
                ),
                "{bad:?}"
            );
        }
        let max = "a".repeat(MAX_CAPABILITY_LEN).leak();
        validate([&Capability::__declare(max, "m")]).unwrap();
        validate([&Capability::__declare("single", "m")]).unwrap();
    }
}
