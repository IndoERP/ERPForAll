//! The store schema: models, fields, kinds, sync classes (3, spec §4.2).

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;

use crate::error::WireError;
use crate::limits::{Limit, WireLimits};
use crate::value::write_string;
use crate::version::SchemaGeneration;

/// Longest model or field name in bytes.
const MAX_NAME_BYTES: usize = 128;

fn bad_name(path: &str, reason: &str) -> WireError {
    WireError::BadName {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn bad_schema(path: &str, reason: impl Into<String>) -> WireError {
    WireError::BadSchema {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

/// A validated model name: lowercase letters, digits, `_` and `.`, starting with a letter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelName(String);

impl ModelName {
    /// Validate a model name.
    ///
    /// # Errors
    /// [`WireError::BadName`] (path `model`).
    pub fn new(name: &str) -> Result<Self, WireError> {
        Self::new_at(name, "model")
    }

    pub(crate) fn new_at(name: &str, path: &str) -> Result<Self, WireError> {
        let valid = name.len() <= MAX_NAME_BYTES
            && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.'));
        if !valid {
            return Err(bad_name(
                path,
                "model name must be lowercase letters, digits, '_' and '.', starting with a letter",
            ));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ModelName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated field name: lowercase letters, digits and `_`, not `id` and not `delete`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldName(String);

impl FieldName {
    /// Validate a field name.
    ///
    /// # Errors
    /// [`WireError::BadName`] (path `field`).
    pub fn new(name: &str) -> Result<Self, WireError> {
        Self::new_at(name, "field")
    }

    pub(crate) fn new_at(name: &str, path: &str) -> Result<Self, WireError> {
        let valid = name.len() <= MAX_NAME_BYTES
            && name
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_')
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err(bad_name(
                path,
                "field name must be lowercase letters, digits and '_', starting with a letter or '_'",
            ));
        }
        if name == "id" || name == "delete" {
            return Err(bad_name(path, "field name is reserved"));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for FieldName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The closed set of field kinds. Adding a kind bumps the wire major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldKind {
    /// `true` or `false`.
    Bool,
    /// A safe integer.
    Int,
    /// A finite number.
    Float,
    /// Text.
    String,
    /// Text typed as markup by the schema.
    Html,
    /// `YYYY-MM-DD`.
    Date,
    /// `YYYY-MM-DD HH:MM:SS`, UTC.
    Datetime,
    /// A selection key.
    Selection,
    /// An id of the target model, or null.
    Many2One,
    /// A `{model, id}` pair naming a schema model, or null.
    Reference,
    /// A set of ids of the target model.
    X2Many,
    /// An integer changed by deltas.
    Counter,
}

impl FieldKind {
    /// The wire name (`bool`, `int`, ..., `x2many`, `counter`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FieldKind::Bool => "bool",
            FieldKind::Int => "int",
            FieldKind::Float => "float",
            FieldKind::String => "string",
            FieldKind::Html => "html",
            FieldKind::Date => "date",
            FieldKind::Datetime => "datetime",
            FieldKind::Selection => "selection",
            FieldKind::Many2One => "many2one",
            FieldKind::Reference => "reference",
            FieldKind::X2Many => "x2many",
            FieldKind::Counter => "counter",
        }
    }

    /// The kind with that wire name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        const ALL: [FieldKind; 12] = [
            FieldKind::Bool,
            FieldKind::Int,
            FieldKind::Float,
            FieldKind::String,
            FieldKind::Html,
            FieldKind::Date,
            FieldKind::Datetime,
            FieldKind::Selection,
            FieldKind::Many2One,
            FieldKind::Reference,
            FieldKind::X2Many,
            FieldKind::Counter,
        ];
        ALL.into_iter().find(|k| k.name() == name)
    }
}

/// How a field's values reach the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyncClass {
    /// Absolute values; writers are serialized on a row.
    State,
    /// An integer changed by deltas.
    Counter,
    /// Values only from answers; events may not carry the field.
    AnswerOnly,
}

/// The declaration of one field before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    kind: FieldKind,
    target: Option<String>,
    answer_only: bool,
    cap: Option<u64>,
}

impl FieldSpec {
    /// A field of `kind` with no target, state sync and no cap.
    #[must_use]
    pub fn new(kind: FieldKind) -> Self {
        Self {
            kind,
            target: None,
            answer_only: false,
            cap: None,
        }
    }

    /// Name the target model (many2one and x2many only).
    #[must_use]
    pub fn target(mut self, model: &str) -> Self {
        self.target = Some(model.to_owned());
        self
    }

    /// Declare the field `answer_only`.
    #[must_use]
    pub fn answer_only(mut self) -> Self {
        self.answer_only = true;
        self
    }

    /// Declare the saturation cap of a counter.
    #[must_use]
    pub fn cap(mut self, cap: u64) -> Self {
        self.cap = Some(cap);
        self
    }
}

/// The sync declaration as written, before it is checked against the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncDecl {
    Unset,
    State,
    AnswerOnly,
}

/// Check one field declaration against the rules of spec §4.2. `path` is `models.<m>.fields.<f>`.
pub(crate) fn make_field(
    path: &str,
    kind: FieldKind,
    target: Option<&str>,
    sync: SyncDecl,
    cap: Option<u64>,
) -> Result<FieldSchema, WireError> {
    let relation = matches!(kind, FieldKind::Many2One | FieldKind::X2Many);
    let target = match (relation, target) {
        (true, None) => {
            return Err(bad_schema(
                &format!("{path}.target"),
                format!("{} needs a target model", kind.name()),
            ));
        }
        (false, Some(_)) => {
            return Err(bad_schema(
                &format!("{path}.target"),
                "target is only allowed on many2one and x2many",
            ));
        }
        (true, Some(name)) => Some(ModelName::new_at(name, &format!("{path}.target"))?),
        (false, None) => None,
    };
    if let Some(cap) = cap {
        if kind != FieldKind::Counter {
            return Err(bad_schema(
                &format!("{path}.cap"),
                "cap is only allowed on counter",
            ));
        }
        if cap == 0 || cap > crate::value::MAX_SAFE as u64 {
            return Err(bad_schema(&format!("{path}.cap"), "cap must be 1..=2^53-1"));
        }
    }
    let sync = match (kind, sync) {
        (FieldKind::Counter, SyncDecl::Unset) => SyncClass::Counter,
        (FieldKind::Counter, _) => {
            return Err(bad_schema(
                &format!("{path}.sync"),
                "sync is not allowed on counter",
            ));
        }
        (_, SyncDecl::AnswerOnly) => SyncClass::AnswerOnly,
        (_, SyncDecl::Unset | SyncDecl::State) => SyncClass::State,
    };
    Ok(FieldSchema {
        kind,
        target,
        sync,
        cap,
    })
}

/// A validated field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSchema {
    kind: FieldKind,
    target: Option<ModelName>,
    sync: SyncClass,
    cap: Option<u64>,
}

impl FieldSchema {
    /// The kind.
    #[must_use]
    pub fn kind(&self) -> FieldKind {
        self.kind
    }

    /// The target model of a many2one or x2many.
    #[must_use]
    pub fn target(&self) -> Option<&ModelName> {
        self.target.as_ref()
    }

    /// The sync class.
    #[must_use]
    pub fn sync(&self) -> SyncClass {
        self.sync
    }

    /// The saturation cap of a counter.
    #[must_use]
    pub fn cap(&self) -> Option<u64> {
        self.cap
    }
}

/// A validated model: its fields by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelSchema {
    fields: BTreeMap<FieldName, FieldSchema>,
}

impl ModelSchema {
    pub(crate) fn insert(&mut self, name: FieldName, field: FieldSchema) {
        self.fields.insert(name, field);
    }

    /// The field with that name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldSchema> {
        self.fields.get(name)
    }

    /// The fields in ascending name order.
    pub fn fields(&self) -> impl Iterator<Item = (&FieldName, &FieldSchema)> {
        self.fields.iter()
    }
}

/// Collects field declarations and validates them into a [`StoreSchema`].
#[derive(Debug)]
pub struct SchemaBuilder {
    generation: SchemaGeneration,
    models: BTreeMap<ModelName, ModelSchema>,
}

impl SchemaBuilder {
    /// Declare a model with no fields yet (a model with no fields is allowed).
    ///
    /// # Errors
    /// [`WireError::BadName`] for a malformed name.
    pub fn model(&mut self, model: &str) -> Result<&mut Self, WireError> {
        let name = ModelName::new_at(model, &format!("models.{model}"))?;
        if !self.models.contains_key(&name) {
            WireLimits::default().check(Limit::Models, self.models.len() + 1, "models")?;
            self.models.insert(name, ModelSchema::default());
        }
        Ok(self)
    }

    /// Declare one field, validating the rules of spec §4.2.
    ///
    /// # Errors
    /// [`WireError::BadName`] or [`WireError::BadSchema`] with the path of the offending member.
    pub fn field(
        &mut self,
        model: &str,
        field: &str,
        spec: FieldSpec,
    ) -> Result<&mut Self, WireError> {
        self.model(model)?;
        let path = format!("models.{model}.fields.{field}");
        let name = FieldName::new_at(field, &path)?;
        let total: usize = self.models.values().map(|m| m.fields.len()).sum();
        WireLimits::default().check(
            Limit::SchemaFields,
            total + 1,
            &format!("models.{model}.fields"),
        )?;
        let sync = if spec.answer_only {
            SyncDecl::AnswerOnly
        } else {
            SyncDecl::Unset
        };
        let made = make_field(&path, spec.kind, spec.target.as_deref(), sync, spec.cap)?;
        let Some(entry) = self.models.get_mut(model) else {
            return Err(bad_schema(
                &format!("models.{model}"),
                "model vanished while declaring a field",
            ));
        };
        if entry.fields.contains_key(&name) {
            return Err(bad_schema(&path, "field declared twice"));
        }
        entry.fields.insert(name, made);
        Ok(self)
    }

    /// Check that every target names a model and produce the schema.
    ///
    /// # Errors
    /// [`WireError::BadSchema`] for a target that is not a model of the schema.
    pub fn build(self) -> Result<StoreSchema, WireError> {
        self.finish()
    }

    pub(crate) fn new(generation: SchemaGeneration) -> Self {
        Self {
            generation,
            models: BTreeMap::new(),
        }
    }

    /// Insert a validated model (decoder path; limits are checked by the caller).
    pub(crate) fn insert_model(&mut self, name: ModelName, model: ModelSchema) {
        self.models.insert(name, model);
    }

    pub(crate) fn finish(self) -> Result<StoreSchema, WireError> {
        for (model, schema) in &self.models {
            for (field, spec) in &schema.fields {
                if let Some(target) = &spec.target
                    && !self.models.contains_key(target)
                {
                    return Err(bad_schema(
                        &format!("models.{model}.fields.{field}.target"),
                        format!("target {target} is not a model of the schema"),
                    ));
                }
            }
        }
        Ok(StoreSchema {
            generation: self.generation,
            models: self.models,
        })
    }
}

/// A validated schema: the generation token and the models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSchema {
    generation: SchemaGeneration,
    models: BTreeMap<ModelName, ModelSchema>,
}

impl StoreSchema {
    /// Start a schema of the given generation.
    #[must_use]
    pub fn builder(generation: SchemaGeneration) -> SchemaBuilder {
        SchemaBuilder::new(generation)
    }

    /// The generation token.
    #[must_use]
    pub fn generation(&self) -> &SchemaGeneration {
        &self.generation
    }

    /// The model with that name.
    #[must_use]
    pub fn model(&self, name: &str) -> Option<&ModelSchema> {
        self.models.get(name)
    }

    /// The models in ascending name order.
    pub fn models(&self) -> impl Iterator<Item = (&ModelName, &ModelSchema)> {
        self.models.iter()
    }

    /// The canonical bytes of the schema payload (the content of the schema topic).
    ///
    /// # Errors
    /// [`WireError::LimitExceeded`] when the output is above the `bytes` limit.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = String::from("{\"models\":{");
        for (i, (model, schema)) in self.models.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_string(&mut out, model.as_str());
            out.push_str(":{\"fields\":{");
            for (j, (field, spec)) in schema.fields.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_string(&mut out, field.as_str());
                out.push_str(":{");
                if let Some(cap) = spec.cap {
                    out.push_str(&format!("\"cap\":{cap},"));
                }
                out.push_str(&format!("\"kind\":\"{}\"", spec.kind.name()));
                if spec.sync == SyncClass::AnswerOnly {
                    out.push_str(",\"sync\":\"answer_only\"");
                }
                if let Some(target) = &spec.target {
                    out.push_str(",\"target\":");
                    write_string(&mut out, target.as_str());
                }
                out.push('}');
            }
            out.push_str("}}");
        }
        out.push_str("},\"schema\":");
        write_string(&mut out, self.generation.as_str());
        out.push_str(&format!(
            ",\"wire\":{}}}",
            crate::version::WireMajor::CURRENT.get()
        ));
        WireLimits::default().check(Limit::Bytes, out.len(), "")?;
        Ok(out.into_bytes())
    }
}
