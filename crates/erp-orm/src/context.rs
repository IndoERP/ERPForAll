//! The environment context: typed fields for the keys the ORM interprets, plus an
//! ordered map for everything else. JSON-2 and Luau see the merged dictionary ([`Context::to_map`]).

use std::collections::BTreeMap;

use erp_fields::{Error, Result, Value};

/// An immutable-by-convention context; environments share it through `Arc`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Context {
    lang: Option<String>,
    tz: Option<String>,
    active_test: Option<bool>,
    /// `Err` keeps a malformed value, reported when the list is used.
    allowed_company_ids: Option<std::result::Result<Vec<i64>, Value>>,
    prefetch_fields: Option<bool>,
    prefetch_langs: Option<bool>,
    extra: BTreeMap<String, Value>,
}

const TYPED_KEYS: [&str; 6] = [
    "lang",
    "tz",
    "active_test",
    "allowed_company_ids",
    "prefetch_fields",
    "prefetch_langs",
];

impl Context {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(key, value)` pairs.
    pub fn from_pairs<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Self {
        let mut ctx = Self::default();
        for (k, v) in pairs {
            ctx.set(k.into(), v);
        }
        ctx
    }

    /// Set a key (routing ORM keys to their typed slot).
    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        match key.as_str() {
            "lang" => self.lang = text(&value),
            "tz" => self.tz = text(&value),
            "active_test" => self.active_test = Some(value.is_truthy()),
            "allowed_company_ids" => {
                self.allowed_company_ids = match &value {
                    // `None`/`False` behaves like an absent key
                    Value::False => None,
                    Value::List(items) => Some(
                        items
                            .iter()
                            .map(|v| match v {
                                Value::Int(i) if *i > 0 => Some(*i),
                                _ => None,
                            })
                            .collect::<Option<Vec<i64>>>()
                            .ok_or_else(|| value.clone()),
                    ),
                    other => Some(Err(other.clone())),
                }
            }
            "prefetch_fields" => self.prefetch_fields = Some(value.is_truthy()),
            "prefetch_langs" => self.prefetch_langs = Some(value.is_truthy()),
            _ => {
                self.extra.insert(key, value);
            }
        }
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &str) {
        match key {
            "lang" => self.lang = None,
            "tz" => self.tz = None,
            "active_test" => self.active_test = None,
            "allowed_company_ids" => self.allowed_company_ids = None,
            "prefetch_fields" => self.prefetch_fields = None,
            "prefetch_langs" => self.prefetch_langs = None,
            _ => {
                self.extra.remove(key);
            }
        }
    }

    /// The value of a key, as a dynamic value.
    pub fn get(&self, key: &str) -> Option<Value> {
        match key {
            "lang" => self.lang.as_deref().map(Value::str),
            "tz" => self.tz.as_deref().map(Value::str),
            "active_test" => self.active_test.map(Value::Bool),
            "allowed_company_ids" => self.allowed_company_ids.as_ref().map(|ids| match ids {
                Ok(ids) => Value::list(ids.iter().map(|i| Value::Int(*i))),
                Err(raw) => raw.clone(),
            }),
            "prefetch_fields" => self.prefetch_fields.map(Value::Bool),
            "prefetch_langs" => self.prefetch_langs.map(Value::Bool),
            _ => self.extra.get(key).cloned(),
        }
    }

    /// True when the key is set.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// True when the context contains any keys starting with `"default_"`.
    pub fn has_default_keys(&self) -> bool {
        self.extra.keys().any(|k| k.starts_with("default_"))
    }

    /// The field names of the `default_<field>` keys, in key order (the keys' values are the
    /// defaults `create` and `default_get` take from the context).
    pub fn default_field_names(&self) -> impl Iterator<Item = &str> {
        self.extra
            .keys()
            .filter_map(|k| k.strip_prefix("default_"))
            .filter(|name| !name.is_empty())
    }

    /// `lang`.
    pub fn lang(&self) -> Option<&str> {
        self.lang.as_deref()
    }

    /// `tz`.
    pub fn tz(&self) -> Option<&str> {
        self.tz.as_deref()
    }

    /// `active_test` (default true).
    pub fn active_test(&self) -> bool {
        self.active_test.unwrap_or(true)
    }

    /// `allowed_company_ids`: `None` when absent; a malformed value (not a list of positive ids) is
    /// a `Validation` error, never "no restriction".
    pub fn allowed_company_ids(&self) -> Result<Option<&[i64]>> {
        match &self.allowed_company_ids {
            None => Ok(None),
            Some(Ok(ids)) => Ok(Some(ids)),
            Some(Err(raw)) => Err(Error::validation(format!(
                "Invalid allowed_company_ids in context: {raw} (expected a list of company ids)"
            ))),
        }
    }

    /// The active company id (the first element of `allowed_company_ids`), or None.
    pub fn company_id(&self) -> Result<Option<i64>> {
        Ok(self
            .allowed_company_ids()?
            .and_then(|ids| ids.first().copied()))
    }

    /// Whether `other` names the same `allowed_company_ids` (absent, malformed and listed values
    /// compared as stored), so an environment can keep its derived company context.
    pub(crate) fn same_company_ids(&self, other: &Context) -> bool {
        self.allowed_company_ids == other.allowed_company_ids
    }

    /// Clone with `allowed_company_ids = [company_id]`.
    pub fn with_company_id(&self, company_id: i64) -> Self {
        self.with("allowed_company_ids", Value::list([Value::Int(company_id)]))
    }

    /// Clone with a key set.
    pub fn with(&self, key: impl Into<String>, value: Value) -> Self {
        let mut ctx = self.clone();
        ctx.set(key, value);
        ctx
    }

    /// `prefetch_fields` (default true).
    pub fn prefetch_fields(&self) -> bool {
        self.prefetch_fields.unwrap_or(true)
    }

    /// `prefetch_langs` (default false).
    pub fn prefetch_langs(&self) -> bool {
        self.prefetch_langs.unwrap_or(false)
    }

    /// Keys that are not interpreted by the ORM.
    pub fn extra(&self) -> &BTreeMap<String, Value> {
        &self.extra
    }

    /// Drop every `default_*` key (turning superuser mode on, ENV §4.2).
    pub(crate) fn without_defaults(&self) -> Self {
        let mut ctx = self.clone();
        ctx.extra.retain(|k, _| !k.starts_with("default_"));
        ctx
    }

    /// Export the merged dictionary only when it fits `max_entries`. Checks before cloning keys.
    /// Zero permits only an empty export. This bounds entries, not nested values or bytes.
    /// Returns a `Validation` error rather than a truncated context.
    ///
    /// ```
    /// use erp_orm::Context;
    /// let context = Context::new();
    /// assert!(context.to_map_bounded(0)?.is_empty());
    /// # Ok::<(), erp_fields::Error>(())
    /// ```
    pub fn to_map_bounded(&self, max_entries: usize) -> Result<BTreeMap<String, Value>> {
        let typed = usize::from(self.lang.is_some())
            + usize::from(self.tz.is_some())
            + usize::from(self.active_test.is_some())
            + usize::from(self.allowed_company_ids.is_some())
            + usize::from(self.prefetch_fields.is_some())
            + usize::from(self.prefetch_langs.is_some());
        let count = self
            .extra
            .len()
            .checked_add(typed)
            .ok_or_else(|| Error::validation("Context entry count overflow"))?;
        if count > max_entries {
            return Err(Error::validation(format!(
                "Context has more than {max_entries} entries"
            )));
        }
        Ok(self.to_map())
    }

    /// The merged dictionary (sorted by key).
    pub fn to_map(&self) -> BTreeMap<String, Value> {
        let mut map = self.extra.clone();
        for key in TYPED_KEYS {
            if let Some(v) = self.get(key) {
                map.insert(key.to_owned(), v);
            }
        }
        map
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_export_preserves_exact_merged_context() {
        assert!(Context::new().to_map_bounded(0).unwrap().is_empty());
        let mut ctx =
            Context::from_pairs([("lang", Value::str("fr_FR")), ("marker", Value::Int(1))]);
        assert_eq!(ctx.to_map_bounded(2).unwrap(), ctx.to_map());
        for cap in [0, 1] {
            let err = ctx.to_map_bounded(cap).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!("Context has more than {cap} entries")
            );
        }
        ctx.set("lang", Value::str("en_US"));
        ctx.set("marker", Value::Int(2));
        assert_eq!(ctx.to_map_bounded(2).unwrap(), ctx.to_map());
        ctx.set("tz", Value::str("UTC"));
        assert!(ctx.to_map_bounded(2).is_err());
        assert_eq!(ctx.to_map_bounded(3).unwrap(), ctx.to_map());
        ctx.remove("lang");
        assert_eq!(ctx.to_map_bounded(2).unwrap(), ctx.to_map());
    }

    #[test]
    fn typed_and_extra_keys() {
        let mut ctx = Context::new();
        ctx.set("lang", Value::str("fr_FR"));
        ctx.set("default_name", Value::str("x"));
        ctx.set(
            "allowed_company_ids",
            Value::list([Value::Int(3), Value::Int(1)]),
        );
        assert_eq!(ctx.lang(), Some("fr_FR"));
        assert_eq!(ctx.allowed_company_ids().unwrap(), Some(&[3, 1][..]));
        assert_eq!(ctx.get("default_name"), Some(Value::str("x")));
        let stripped = ctx.without_defaults();
        assert!(!stripped.contains("default_name"));
        assert_eq!(stripped.lang(), Some("fr_FR"));
        assert_eq!(ctx.to_map().len(), 3);
    }

    #[test]
    fn malformed_company_ids_are_errors() {
        for bad in [
            Value::str("1"),
            Value::list([Value::Int(1), Value::str("x")]),
            Value::list([Value::Int(0)]),
        ] {
            let ctx = Context::from_pairs([("allowed_company_ids", bad)]);
            let err = ctx.allowed_company_ids().unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        }
        let absent = Context::from_pairs([("allowed_company_ids", Value::False)]);
        assert_eq!(absent.allowed_company_ids().unwrap(), None);
    }
}
