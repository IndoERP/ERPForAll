//! API-key scopes (DN §5): which models and methods a key may call at RPC
//! dispatch, and whether it is read-only ORM-wide. The key side (issue, storage, expiry) is
//! 's; this is the value the principal carries.

use super::error::PrincipalError;
use super::ids::{ApiKeyId, truncate};

/// Most model or method names one scope lists.
pub const MAX_SCOPE_ENTRIES: usize = 4_096;
/// Longest model or method name in a scope (bytes).
pub const MAX_SCOPE_NAME_LEN: usize = 256;
/// Longest API key name carried in a scope (bytes).
pub const MAX_KEY_NAME_LEN: usize = 1_024;

/// Models an API key may call (checked at dispatch, not by `check_model`; DN §5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ModelAllow {
    /// Every model.
    All,
    /// These model names (sorted, unique, not empty).
    Named(Box<[Box<str>]>),
}

/// Methods an API key may call (checked at dispatch; DN §5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MethodAllow {
    /// Every method.
    All,
    /// These method names (sorted, unique, not empty).
    Named(Box<[Box<str>]>),
}

impl ModelAllow {
    /// Only `names`.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidScope`] for an empty list, a duplicate, an empty or over-long
    /// name, a name with whitespace or control characters, or more than [`MAX_SCOPE_ENTRIES`]
    /// names.
    pub fn named<S: AsRef<str>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<Self, PrincipalError> {
        named_list("model", names).map(Self::Named)
    }

    /// A stable text form: `*` for all, else the sorted names joined by newlines.
    pub(crate) fn encode(&self) -> String {
        match self {
            Self::All => "*".to_owned(),
            Self::Named(names) => names.join("\n"),
        }
    }

    /// Whether `model` is allowed.
    pub fn allows(&self, model: &str) -> bool {
        match self {
            Self::All => true,
            Self::Named(names) => names.binary_search_by(|n| (**n).cmp(model)).is_ok(),
        }
    }
}

impl MethodAllow {
    /// Only `names`.
    ///
    /// # Errors
    ///
    /// As [`ModelAllow::named`].
    pub fn named<S: AsRef<str>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<Self, PrincipalError> {
        named_list("method", names).map(Self::Named)
    }

    /// A stable text form: `*` for all, else the sorted names joined by newlines.
    pub(crate) fn encode(&self) -> String {
        match self {
            Self::All => "*".to_owned(),
            Self::Named(names) => names.join("\n"),
        }
    }

    /// Whether `method` is allowed.
    pub fn allows(&self, method: &str) -> bool {
        match self {
            Self::All => true,
            Self::Named(names) => names.binary_search_by(|n| (**n).cmp(method)).is_ok(),
        }
    }
}

fn named_list<S: AsRef<str>>(
    what: &str,
    names: impl IntoIterator<Item = S>,
) -> Result<Box<[Box<str>]>, PrincipalError> {
    let invalid = |problem: String| PrincipalError::InvalidScope { problem };
    let mut list: Vec<Box<str>> = Vec::new();
    for name in names {
        let name = name.as_ref();
        if list.len() == MAX_SCOPE_ENTRIES {
            return Err(invalid(format!(
                "more than {MAX_SCOPE_ENTRIES} {what} names"
            )));
        }
        if name.is_empty() {
            return Err(invalid(format!("empty {what} name")));
        }
        if name.len() > MAX_SCOPE_NAME_LEN {
            return Err(invalid(format!(
                "{what} name '{}' is longer than {MAX_SCOPE_NAME_LEN} bytes",
                truncate(name)
            )));
        }
        if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid(format!(
                "{what} name '{}' contains whitespace or control characters",
                truncate(name)
            )));
        }
        list.push(name.into());
    }
    if list.is_empty() {
        return Err(invalid(format!(
            "an empty {what} list allows nothing (revoke the key instead)"
        )));
    }
    list.sort_unstable();
    if let Some(pair) = list.windows(2).find(|w| w[0] == w[1]) {
        return Err(invalid(format!("{what} '{}' is listed twice", pair[0])));
    }
    Ok(list.into_boxed_slice())
}

/// An API key's scope: model and method allowlist (dispatch) and `read_only` (ORM-wide).
///
/// ```
/// use erp_orm::{ApiKeyId, ModelAllow, MethodAllow, Scope};
/// let scope = Scope::builder(ApiKeyId::new(4)?, "invoice bot")
///     .models(ModelAllow::named(["sale.order", "account.move"])?)
///     .methods(MethodAllow::All)
///     .read_only(true)
///     .build()?;
/// assert!(scope.models().allows("sale.order") && !scope.models().allows("res.partner"));
/// assert!(scope.read_only());
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Scope {
    key: ApiKeyId,
    name: Box<str>,
    models: ModelAllow,
    methods: MethodAllow,
    read_only: bool,
}

impl Scope {
    /// Start a scope for key `key` named `name`. Models and methods must be set explicitly.
    pub fn builder(key: ApiKeyId, name: impl Into<String>) -> ScopeBuilder {
        ScopeBuilder {
            key,
            name: name.into(),
            models: None,
            methods: None,
            read_only: false,
        }
    }

    /// The key.
    pub fn key(&self) -> ApiKeyId {
        self.key
    }

    /// The key's name (used in error messages, DN §5).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The model allowlist.
    pub fn models(&self) -> &ModelAllow {
        &self.models
    }

    /// The method allowlist.
    pub fn methods(&self) -> &MethodAllow {
        &self.methods
    }

    /// Whether the key may only read.
    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

/// Builder of a [`Scope`].
#[derive(Debug)]
#[must_use = "call `.build()`"]
pub struct ScopeBuilder {
    key: ApiKeyId,
    name: String,
    models: Option<ModelAllow>,
    methods: Option<MethodAllow>,
    read_only: bool,
}

impl ScopeBuilder {
    /// The model allowlist (required).
    pub fn models(mut self, models: ModelAllow) -> Self {
        self.models = Some(models);
        self
    }

    /// The method allowlist (required).
    pub fn methods(mut self, methods: MethodAllow) -> Self {
        self.methods = Some(methods);
        self
    }

    /// Read-only key (default: not read-only, as an unscoped key).
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The scope.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidScope`] when the models or methods were not set, or the name is
    /// empty or longer than [`MAX_KEY_NAME_LEN`] bytes.
    pub fn build(self) -> Result<Scope, PrincipalError> {
        let invalid = |problem: String| PrincipalError::InvalidScope { problem };
        if self.name.is_empty() {
            return Err(invalid("the key name is empty".to_owned()));
        }
        if self.name.len() > MAX_KEY_NAME_LEN {
            return Err(invalid(format!(
                "the key name is longer than {MAX_KEY_NAME_LEN} bytes"
            )));
        }
        let Some(models) = self.models else {
            return Err(invalid("the model allowlist is not set".to_owned()));
        };
        let Some(methods) = self.methods else {
            return Err(invalid("the method allowlist is not set".to_owned()));
        };
        Ok(Scope {
            key: self.key,
            name: self.name.into_boxed_str(),
            models,
            methods,
            read_only: self.read_only,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ApiKeyId {
        ApiKeyId::new(1).unwrap()
    }

    fn problem(e: PrincipalError) -> String {
        assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
        e.to_string()
    }

    #[test]
    fn named_lists_are_sorted_and_checked() {
        let m = ModelAllow::named(["b.m", "a.m", "ü.m"]).unwrap();
        assert!(m.allows("a.m") && m.allows("b.m") && m.allows("ü.m") && !m.allows("c.m"));
        assert_eq!(
            m,
            ModelAllow::named(["ü.m", "a.m", "b.m"]).unwrap(),
            "order of input does not matter"
        );
        assert!(ModelAllow::All.allows("anything"));
        let cases: [(Vec<&str>, &str); 5] = [
            (
                vec![],
                "invalid API key scope: an empty model list allows nothing (revoke the key instead)",
            ),
            (
                vec!["a", "a"],
                "invalid API key scope: model 'a' is listed twice",
            ),
            (vec![""], "invalid API key scope: empty model name"),
            (
                vec!["a b"],
                "invalid API key scope: model name 'a b' contains whitespace or control characters",
            ),
            (
                vec!["a\r\n"],
                "invalid API key scope: model name 'a\r\n' contains whitespace or control characters",
            ),
        ];
        for (names, message) in cases {
            assert_eq!(problem(ModelAllow::named(names).unwrap_err()), message);
        }
        let long = "x".repeat(MAX_SCOPE_NAME_LEN + 1);
        assert_eq!(
            problem(MethodAllow::named([long.as_str()]).unwrap_err()),
            format!(
                "invalid API key scope: method name '{}…' is longer than 256 bytes",
                "x".repeat(64)
            )
        );
        let many: Vec<String> = (0..=MAX_SCOPE_ENTRIES).map(|i| format!("m{i}")).collect();
        assert_eq!(
            problem(MethodAllow::named(&many).unwrap_err()),
            "invalid API key scope: more than 4096 method names"
        );
        assert!(MethodAllow::named(&many[..MAX_SCOPE_ENTRIES]).is_ok());
    }

    #[test]
    fn builder_requires_both_lists_and_a_name() {
        assert_eq!(
            problem(
                Scope::builder(key(), "k")
                    .methods(MethodAllow::All)
                    .build()
                    .unwrap_err()
            ),
            "invalid API key scope: the model allowlist is not set"
        );
        assert_eq!(
            problem(
                Scope::builder(key(), "k")
                    .models(ModelAllow::All)
                    .build()
                    .unwrap_err()
            ),
            "invalid API key scope: the method allowlist is not set"
        );
        assert_eq!(
            problem(
                Scope::builder(key(), "")
                    .models(ModelAllow::All)
                    .methods(MethodAllow::All)
                    .build()
                    .unwrap_err()
            ),
            "invalid API key scope: the key name is empty"
        );
        assert_eq!(
            problem(
                Scope::builder(key(), "n".repeat(MAX_KEY_NAME_LEN + 1))
                    .models(ModelAllow::All)
                    .methods(MethodAllow::All)
                    .build()
                    .unwrap_err()
            ),
            "invalid API key scope: the key name is longer than 1024 bytes"
        );
        let s = Scope::builder(key(), "clé")
            .models(ModelAllow::All)
            .methods(MethodAllow::named(["read"]).unwrap())
            .build()
            .unwrap();
        assert_eq!((s.key(), s.name(), s.read_only()), (key(), "clé", false));
        assert!(s.methods().allows("read") && !s.methods().allows("write"));
    }
}
