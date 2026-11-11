//! Parameter declarations of a topic and their validation.

use std::collections::BTreeMap;

use erp_sdk::serde_json::{self, Value as Json};
use erp_sdk::{Error, Result, Value};

/// Most bytes of one topic's serialized `params` object.
pub const MAX_PARAMS_BYTES: usize = 4096;
/// Deepest nesting of one topic's `params` (the object itself is level 1).
pub const MAX_PARAMS_DEPTH: usize = 4;

/// The type and bound of one declared parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamKind {
    /// An integer in `min..=max`.
    Int {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// Text of at most `max_bytes` bytes.
    Text {
        /// Longest accepted text in bytes.
        max_bytes: usize,
    },
    /// A boolean.
    Bool,
    /// A list of positive ids, at most `max_items` of them.
    Ids {
        /// Longest accepted list.
        max_items: usize,
    },
}

/// One declared parameter of a topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParamDecl {
    name: &'static str,
    kind: ParamKind,
    required: bool,
}

impl ParamDecl {
    const fn new(name: &'static str, kind: ParamKind) -> Self {
        Self {
            name,
            kind,
            required: true,
        }
    }

    /// A required integer in `min..=max`.
    #[must_use]
    pub const fn int(name: &'static str, min: i64, max: i64) -> Self {
        Self::new(name, ParamKind::Int { min, max })
    }

    /// A required text of at most `max_bytes` bytes.
    #[must_use]
    pub const fn text(name: &'static str, max_bytes: usize) -> Self {
        Self::new(name, ParamKind::Text { max_bytes })
    }

    /// A required boolean.
    #[must_use]
    pub const fn boolean(name: &'static str) -> Self {
        Self::new(name, ParamKind::Bool)
    }

    /// A required list of at most `max_items` positive ids.
    #[must_use]
    pub const fn ids(name: &'static str, max_items: usize) -> Self {
        Self::new(name, ParamKind::Ids { max_items })
    }

    /// Make the parameter optional.
    #[must_use]
    pub const fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// The parameter name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The parameter's type and bound.
    #[must_use]
    pub const fn kind(&self) -> ParamKind {
        self.kind
    }

    /// Whether a call must pass it.
    #[must_use]
    pub const fn is_required(&self) -> bool {
        self.required
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ParamValue {
    Int(i64),
    Text(String),
    Bool(bool),
    Ids(Vec<i64>),
}

/// The validated parameters of one topic run. Every value satisfied its declaration; asking for a
/// name the topic did not declare, or with the wrong type, is a `Programming` error of the topic.
#[derive(Debug, Clone, Default)]
pub struct TopicParams {
    values: BTreeMap<&'static str, ParamValue>,
}

impl TopicParams {
    fn get(&self, name: &str) -> Option<&ParamValue> {
        self.values.get(name)
    }

    /// Whether the caller passed the parameter `name` (a declared one that was sent).
    #[must_use]
    pub fn is_set(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    fn missing(name: &str) -> Error {
        Error::programming(format!(
            "topic parameter '{name}' is required but was not passed, or was not declared"
        ))
    }

    fn wrong_type(name: &str, want: &str) -> Error {
        Error::programming(format!("topic parameter '{name}' is not {want}"))
    }

    /// A required integer parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is not a declared integer that was passed.
    pub fn int(&self, name: &str) -> Result<i64> {
        self.opt_int(name)?.ok_or_else(|| Self::missing(name))
    }

    /// An optional integer parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is declared with another type.
    pub fn opt_int(&self, name: &str) -> Result<Option<i64>> {
        match self.get(name) {
            None => Ok(None),
            Some(ParamValue::Int(v)) => Ok(Some(*v)),
            Some(_) => Err(Self::wrong_type(name, "an integer")),
        }
    }

    /// A required text parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is not a declared text that was passed.
    pub fn text(&self, name: &str) -> Result<&str> {
        self.opt_text(name)?.ok_or_else(|| Self::missing(name))
    }

    /// An optional text parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is declared with another type.
    pub fn opt_text(&self, name: &str) -> Result<Option<&str>> {
        match self.get(name) {
            None => Ok(None),
            Some(ParamValue::Text(v)) => Ok(Some(v)),
            Some(_) => Err(Self::wrong_type(name, "text")),
        }
    }

    /// An optional boolean parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is declared with another type.
    pub fn opt_boolean(&self, name: &str) -> Result<Option<bool>> {
        match self.get(name) {
            None => Ok(None),
            Some(ParamValue::Bool(v)) => Ok(Some(*v)),
            Some(_) => Err(Self::wrong_type(name, "a boolean")),
        }
    }

    /// An optional id-list parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is declared with another type.
    pub fn opt_ids(&self, name: &str) -> Result<Option<&[i64]>> {
        match self.get(name) {
            None => Ok(None),
            Some(ParamValue::Ids(v)) => Ok(Some(v)),
            Some(_) => Err(Self::wrong_type(name, "an id list")),
        }
    }

    /// A required boolean parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is not a declared boolean that was passed.
    pub fn boolean(&self, name: &str) -> Result<bool> {
        match self.get(name) {
            Some(ParamValue::Bool(v)) => Ok(*v),
            None => Err(Self::missing(name)),
            Some(_) => Err(Self::wrong_type(name, "a boolean")),
        }
    }

    /// A required id-list parameter.
    ///
    /// # Errors
    /// `Programming` when `name` is not a declared id list that was passed.
    pub fn ids(&self, name: &str) -> Result<&[i64]> {
        match self.get(name) {
            Some(ParamValue::Ids(v)) => Ok(v),
            None => Err(Self::missing(name)),
            Some(_) => Err(Self::wrong_type(name, "an id list")),
        }
    }
}

/// An RPC value as JSON, bounded in depth; `False` is JSON `null` (the JSON-2 binder's reading of it).
pub(crate) fn to_json(value: &Value, depth: usize, what: &str) -> Result<Json> {
    if depth > MAX_PARAMS_DEPTH + 2 {
        return Err(Error::validation(format!("{what} is nested too deeply")));
    }
    Ok(match value {
        Value::False => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or_else(|| Error::validation(format!("{what} holds a non-finite number")))?,
        Value::Str(s) => Json::String(s.to_string()),
        Value::List(items) => Json::Array(
            items
                .iter()
                .map(|item| to_json(item, depth + 1, what))
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Map(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, item) in map.iter() {
                out.insert(key.clone(), to_json(item, depth + 1, what)?);
            }
            Json::Object(out)
        }
        Value::Json(json) => (**json).clone(),
        _ => {
            return Err(Error::validation(format!(
                "{what} holds a value that is not JSON"
            )));
        }
    })
}

fn json_depth(value: &Json) -> usize {
    match value {
        Json::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        Json::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Check `params` against the declarations of `topic`: an object (or absent), within the size and
/// depth bounds, with only declared names, every required one present, each of its type and bound.
pub(crate) fn validate(
    topic: &str,
    decls: &'static [ParamDecl],
    params: Option<&Json>,
) -> Result<TopicParams> {
    let empty = serde_json::Map::new();
    let object = match params {
        None => &empty,
        Some(Json::Object(object)) => object,
        Some(_) => {
            return Err(Error::validation(format!(
                "store topic '{topic}': params must be an object"
            )));
        }
    };
    if let Some(params) = params {
        let size = params.to_string().len();
        if size > MAX_PARAMS_BYTES {
            return Err(Error::validation(format!(
                "store topic '{topic}': params are {size} bytes; at most {MAX_PARAMS_BYTES} are accepted"
            )));
        }
        if json_depth(params) > MAX_PARAMS_DEPTH {
            return Err(Error::validation(format!(
                "store topic '{topic}': params are nested deeper than {MAX_PARAMS_DEPTH} levels"
            )));
        }
    }
    for key in object.keys() {
        if !decls.iter().any(|d| d.name == key) {
            return Err(Error::validation(format!(
                "store topic '{topic}': unknown param '{}'",
                bounded(key)
            )));
        }
    }
    let mut values = BTreeMap::new();
    for decl in decls {
        let Some(raw) = object.get(decl.name) else {
            if decl.required {
                return Err(Error::validation(format!(
                    "store topic '{topic}': param '{}' is required",
                    decl.name
                )));
            }
            continue;
        };
        values.insert(decl.name, check_value(topic, decl, raw)?);
    }
    Ok(TopicParams { values })
}

fn check_value(topic: &str, decl: &ParamDecl, raw: &Json) -> Result<ParamValue> {
    let bad = |why: String| {
        Error::validation(format!(
            "store topic '{topic}': param '{}' {why}",
            decl.name
        ))
    };
    match (decl.kind, raw) {
        (ParamKind::Int { min, max }, Json::Number(n)) => match n.as_i64() {
            Some(v) if (min..=max).contains(&v) => Ok(ParamValue::Int(v)),
            Some(v) => Err(bad(format!("is {v}; it must be between {min} and {max}"))),
            None => Err(bad("is not an integer".to_owned())),
        },
        (ParamKind::Text { max_bytes }, Json::String(s)) => {
            if s.len() > max_bytes {
                Err(bad(format!(
                    "is {} bytes; at most {max_bytes} are accepted",
                    s.len()
                )))
            } else {
                Ok(ParamValue::Text(s.clone()))
            }
        }
        (ParamKind::Bool, Json::Bool(b)) => Ok(ParamValue::Bool(*b)),
        (ParamKind::Ids { max_items }, Json::Array(items)) => {
            if items.len() > max_items {
                return Err(bad(format!(
                    "has {} ids; at most {max_items} are accepted",
                    items.len()
                )));
            }
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item.as_i64() {
                    Some(id) if id > 0 => ids.push(id),
                    _ => return Err(bad("holds a value that is not a positive id".to_owned())),
                }
            }
            Ok(ParamValue::Ids(ids))
        }
        (kind, _) => Err(bad(format!("must be {}", expected(kind)))),
    }
}

fn expected(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Int { .. } => "an integer",
        ParamKind::Text { .. } => "text",
        ParamKind::Bool => "a boolean",
        ParamKind::Ids { .. } => "a list of ids",
    }
}

/// A caller-supplied name cut to 64 bytes (on a character boundary) with an ellipsis, so an error
/// never echoes unbounded input.
pub(crate) fn bounded(text: &str) -> String {
    if text.len() <= 64 {
        return text.to_owned();
    }
    let mut end = 64;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
