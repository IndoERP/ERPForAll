//! The keys of a row batch, held once per batch.

use std::fmt;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::{Error, Result};

/// Layouts with at most this many keys find a key by a linear scan; larger ones use a hash map
/// (design note §2: a scan over a few short keys beats hashing).
const LINEAR_LOOKUP_MAX: usize = 16;

/// Most keys a layout holds (65,535): a key position fits a `u16`.
pub const MAX_KEYS: usize = u16::MAX as usize;

/// Most keys named in an error message; a longer list ends with `… and N more`.
const KEYS_SHOWN: usize = 20;

/// A key of a row: a registry field name, or a key stored once in the layout (a group-by spec,
/// `__extra_domain`, a key an override adds).
///
/// Two keys are equal and hash alike when their text is the same, whichever variant holds it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Key {
    /// A name with static lifetime, e.g. `FieldSpec::name` from the registry.
    Field(&'static str),
    /// Any other key, stored once per layout.
    Owned(Arc<str>),
}

impl Key {
    /// The key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Field(name) => name,
            Self::Owned(name) => name,
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Key {}

impl std::hash::Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl From<&'static str> for Key {
    fn from(name: &'static str) -> Self {
        Self::Field(name)
    }
}

impl From<Arc<str>> for Key {
    fn from(name: Arc<str>) -> Self {
        Self::Owned(name)
    }
}

impl From<String> for Key {
    fn from(name: String) -> Self {
        Self::Owned(Arc::from(name))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shared by every row of a batch through an `Arc`; no row owns a key.
#[derive(Clone)]
pub struct Layout {
    model: Option<Key>,
    keys: Box<[Key]>,
    /// `"key":` for every key, pre-escaped, back to back.
    json: Box<[u8]>,
    /// Per key, its bytes in `json` as `(start, end)`.
    spans: Box<[(u32, u32)]>,
    /// Key → position, only above [`LINEAR_LOOKUP_MAX`] keys.
    index: Option<FxHashMap<Box<str>, u16>>,
}

impl Layout {
    /// A layout of `keys` without a model name.
    ///
    /// # Errors
    /// `Programming` for a duplicate key or more than [`MAX_KEYS`] keys.
    ///
    /// ```
    /// use erp_fields::rows::{Key, Layout};
    /// let layout = Layout::new([Key::from("id"), Key::from("name")]).unwrap();
    /// assert_eq!(layout.position("name"), Some(1));
    /// assert_eq!(layout.json_key(1), Some(&br#""name":"#[..]));
    /// ```
    pub fn new(keys: impl IntoIterator<Item = Key>) -> Result<Arc<Self>> {
        Self::build(None, keys.into_iter().collect()).map(Arc::new)
    }

    /// A layout of `keys` for rows of `model` (named in error messages).
    ///
    /// # Errors
    /// As [`Layout::new`].
    pub fn for_model(
        model: impl Into<Key>,
        keys: impl IntoIterator<Item = Key>,
    ) -> Result<Arc<Self>> {
        Self::build(Some(model.into()), keys.into_iter().collect()).map(Arc::new)
    }

    pub(crate) fn build(model: Option<Key>, keys: Vec<Key>) -> Result<Self> {
        if keys.len() > MAX_KEYS {
            return Err(Error::programming(format!(
                "a row layout holds at most {MAX_KEYS} keys, not {}",
                keys.len()
            )));
        }
        let mut json = Vec::with_capacity(keys.iter().map(|k| k.as_str().len() + 3).sum());
        let mut spans = Vec::with_capacity(keys.len());
        for key in &keys {
            let start = json.len();
            crate::json::write_string(&mut json, key.as_str());
            json.push(b':');
            spans.push((to_u32(start)?, to_u32(json.len())?));
        }
        let index = if keys.len() > LINEAR_LOOKUP_MAX {
            let mut index = FxHashMap::default();
            index.reserve(keys.len());
            for (pos, key) in keys.iter().enumerate() {
                // `pos` < MAX_KEYS = u16::MAX, checked above
                let pos = u16::try_from(pos).map_err(|_| too_many(keys.len()))?;
                if index.insert(Box::from(key.as_str()), pos).is_some() {
                    return Err(duplicate(model.as_ref(), key.as_str()));
                }
            }
            Some(index)
        } else {
            for (pos, key) in keys.iter().enumerate() {
                if keys[..pos].iter().any(|k| k.as_str() == key.as_str()) {
                    return Err(duplicate(model.as_ref(), key.as_str()));
                }
            }
            None
        };
        Ok(Self {
            model,
            keys: keys.into_boxed_slice(),
            json: json.into_boxed_slice(),
            spans: spans.into_boxed_slice(),
            index,
        })
    }

    /// The model the rows belong to, when known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_ref().map(Key::as_str)
    }

    /// How many keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the layout has no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The keys in order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.keys.iter().map(Key::as_str)
    }

    /// The key at `pos`.
    #[must_use]
    pub fn key(&self, pos: usize) -> Option<&str> {
        self.keys.get(pos).map(Key::as_str)
    }

    /// The position of `key`.
    #[must_use]
    pub fn position(&self, key: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(key).map(|pos| usize::from(*pos)),
            None => self.keys.iter().position(|k| k.as_str() == key),
        }
    }

    /// The key at `pos` as JSON, with its colon: `"key":`, escaped once when the layout was
    /// built.
    #[must_use]
    pub fn json_key(&self, pos: usize) -> Option<&[u8]> {
        let (start, end) = *self.spans.get(pos)?;
        self.json.get(start as usize..end as usize)
    }

    /// The layout with `key` added at the end.
    pub(crate) fn with_key(&self, key: Key) -> Result<Self> {
        let mut keys = self.keys.to_vec();
        keys.push(key);
        Self::build(self.model.clone(), keys)
    }

    /// The layout without the key at `pos`.
    pub(crate) fn without(&self, pos: usize) -> Result<Self> {
        let mut keys = self.keys.to_vec();
        if pos >= keys.len() {
            return Err(Error::programming(format!(
                "key position {pos} is out of range ({} keys)",
                keys.len()
            )));
        }
        keys.remove(pos);
        Self::build(self.model.clone(), keys)
    }

    /// ` of <model> rows` or nothing, for error messages (formatted only when an error is).
    pub(crate) fn of_model(&self) -> ModelSuffix<'_> {
        ModelSuffix {
            preposition: "of",
            model: self.model.as_ref(),
        }
    }

    /// ` in <model> rows` or nothing, for error messages (formatted only when an error is).
    pub(crate) fn in_model(&self) -> ModelSuffix<'_> {
        ModelSuffix {
            preposition: "in",
            model: self.model.as_ref(),
        }
    }

    pub(crate) fn unknown_key(&self, key: &str) -> Error {
        let subject = match &self.model {
            Some(model) => format!("{model} row"),
            None => "row".to_owned(),
        };
        Error::programming(format!(
            "{subject} has no key '{key}' (keys: {})",
            self.shown_keys()
        ))
    }

    /// The keys for an error message, at most [`KEYS_SHOWN`] of them.
    fn shown_keys(&self) -> String {
        let mut shown = self
            .keys
            .iter()
            .take(KEYS_SHOWN)
            .map(Key::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if self.keys.len() > KEYS_SHOWN {
            shown.push_str(&format!(" … and {} more", self.keys.len() - KEYS_SHOWN));
        }
        if self.keys.is_empty() {
            shown.push_str("none");
        }
        shown
    }
}

impl fmt::Debug for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layout")
            .field("model", &self.model())
            .field("keys", &self.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PartialEq for Layout {
    /// Same model and same keys in the same order.
    fn eq(&self, other: &Self) -> bool {
        self.model() == other.model() && self.keys().eq(other.keys())
    }
}

/// ` of <model> rows` in an error message, or nothing without a model; allocates nothing until
/// it is formatted.
#[derive(Clone, Copy)]
pub(crate) struct ModelSuffix<'a> {
    preposition: &'static str,
    model: Option<&'a Key>,
}

impl fmt::Display for ModelSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.model {
            Some(model) => write!(f, " {} {model} rows", self.preposition),
            None => Ok(()),
        }
    }
}

fn to_u32(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| {
        Error::programming(format!(
            "the keys of a row layout take {n} bytes of JSON, more than {} allowed",
            u32::MAX
        ))
    })
}

fn too_many(n: usize) -> Error {
    Error::programming(format!(
        "a row layout holds at most {MAX_KEYS} keys, not {n}"
    ))
}

fn duplicate(model: Option<&Key>, key: &str) -> Error {
    match model {
        Some(model) => Error::programming(format!(
            "duplicate key '{key}' in the layout of {model} rows"
        )),
        None => Error::programming(format!("duplicate key '{key}' in a row layout")),
    }
}
