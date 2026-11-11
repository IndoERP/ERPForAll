//! The declaration of a store topic and what its run sees and produces (2).

use std::future::Future;
use std::pin::Pin;

use erp_sdk::serde_json::Value as Json;
use erp_sdk::store_wire::{FieldSpec, FieldValue};
use erp_sdk::{Env, Result, Transaction};

use super::params::{ParamDecl, TopicParams};

/// A boxed topic future.
pub type TopicFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// The run of a topic: reads through `tx`, stages its answer in the output.
pub type RunFn =
    for<'a> fn(&'a mut Transaction, &'a TopicInput<'a>, &'a mut TopicOutput) -> TopicFuture<'a>;

/// The record-level access check of a topic, run after its audience and before its run. An
/// `Access` error denies the topic; any other error fails the call.
pub type AccessFn = for<'a> fn(&'a mut Transaction, &'a TopicInput<'a>) -> TopicFuture<'a>;

/// Who may request a topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Audience {
    Internal,
    LoggedIn,
}

/// What a topic run sees: the caller's environment and its validated parameters.
#[derive(Debug)]
pub struct TopicInput<'a> {
    env: &'a Env,
    params: &'a TopicParams,
}

impl<'a> TopicInput<'a> {
    pub(crate) fn new(env: &'a Env, params: &'a TopicParams) -> Self {
        Self { env, params }
    }

    /// The caller's environment (user, companies, context).
    #[must_use]
    pub fn env(&self) -> &Env {
        self.env
    }

    /// The validated parameters.
    #[must_use]
    pub fn params(&self) -> &TopicParams {
        self.params
    }
}

pub(crate) enum StagedOp {
    Set {
        model: String,
        id: u64,
        field: String,
        value: FieldValue,
    },
    Delete {
        model: String,
        id: u64,
    },
}

/// What a topic run produced: store records and an optional document. Nothing reaches the answer
/// until the whole run, including the access error path, has succeeded.
#[derive(Default)]
pub struct TopicOutput {
    pub(crate) ops: Vec<StagedOp>,
    pub(crate) data: Option<Json>,
}

impl std::fmt::Debug for TopicOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TopicOutput")
            .field("ops", &self.ops.len())
            .field("data", &self.data.is_some())
            .finish()
    }
}

impl TopicOutput {
    /// Stage one field of one record. Checked against the schema when the runner applies it.
    pub fn set(&mut self, model: &str, id: u64, field: &str, value: FieldValue) {
        self.ops.push(StagedOp::Set {
            model: model.to_owned(),
            id,
            field: field.to_owned(),
            value,
        });
    }

    /// Stage a delete marker.
    pub fn delete(&mut self, model: &str, id: u64) {
        self.ops.push(StagedOp::Delete {
            model: model.to_owned(),
            id,
        });
    }

    /// Set the document returned in the topic's slot (`data`), for a topic that is not store records.
    pub fn data(&mut self, document: Json) {
        self.data = Some(document);
    }
}

/// A topic of the closed registry: name, parameters, audience, access check and statement budget.
/// Declare it with the [`store_topic!`](crate::store_topic) macro, which contributes it to the
/// `mail.store_topic` slot of `erp_sdk::extensions::EXTENSIONS`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct StoreTopic {
    pub(crate) addon: &'static str,
    pub(crate) name: &'static str,
    pub(crate) params: &'static [ParamDecl],
    pub(crate) audience: Audience,
    pub(crate) access: Option<AccessFn>,
    pub(crate) statements: u32,
    pub(crate) extra: &'static [(&'static str, u32)],
    pub(crate) needs_groups: bool,
    pub(crate) run: RunFn,
}

impl StoreTopic {
    /// A topic of `addon` named `name` (`a-z0-9_.`, starting with a letter, at most 64 bytes),
    /// for internal users, with no parameters and a budget of zero statements.
    #[must_use]
    pub const fn new(addon: &'static str, name: &'static str, run: RunFn) -> Self {
        Self {
            addon,
            name,
            params: &[],
            audience: Audience::Internal,
            access: None,
            statements: 0,
            extra: &[],
            needs_groups: false,
            run,
        }
    }

    /// Declare the parameters.
    #[must_use]
    pub const fn params(mut self, params: &'static [ParamDecl]) -> Self {
        self.params = params;
        self
    }

    /// Set the audience.
    #[must_use]
    pub const fn audience(mut self, audience: Audience) -> Self {
        self.audience = audience;
        self
    }

    /// Add a record-level access check.
    #[must_use]
    pub const fn access(mut self, access: AccessFn) -> Self {
        self.access = Some(access);
        self
    }

    /// The most statements the access check and the run may send together.
    #[must_use]
    pub const fn statements(mut self, statements: u32) -> Self {
        self.statements = statements;
        self
    }

    /// Statements a topic may send **besides** [`Self::statements`] when the caller passes the
    /// named optional parameter: `(parameter, extra statements)`. For a parameter that selects a
    /// costlier shape of the same topic (for example a cursor that needs one more search); a topic
    /// states the ceiling of each shape instead of raising the ceiling of all of them. The
    /// registry refuses an entry that names a parameter the topic does not declare as optional,
    /// names one twice or adds 0 statements.
    #[must_use]
    pub const fn statements_plus(mut self, extra: &'static [(&'static str, u32)]) -> Self {
        self.extra = extra;
        self
    }

    /// Declare that the topic reads the caller's groups (its standing, a group check). The
    /// runner then resolves the caller's groups for the topic after the audience check, once per
    /// call, outside the topic's statement count: the groups are read lazily by the first group
    /// check of a transaction, and whichever topic came first would otherwise pay that statement
    /// out of its own budget. The statement is still sent, once per call, and is the only one
    /// moved out of a topic's count, so a whole call sends the topics' statements plus that one.
    #[must_use]
    pub const fn needs_groups(mut self) -> Self {
        self.needs_groups = true;
        self
    }

    /// The addon that registers the topic.
    #[must_use]
    pub const fn addon_name(&self) -> &'static str {
        self.addon
    }

    /// The topic name.
    #[must_use]
    pub const fn topic_name(&self) -> &'static str {
        self.name
    }

    /// The statement budget of the base shape of the topic.
    #[must_use]
    pub const fn budget(&self) -> u32 {
        self.statements
    }

    /// The statement budget of one call: [`Self::budget`] plus the extra statements of every
    /// parameter of [`Self::statements_plus`] that `params` holds.
    #[must_use]
    pub fn budget_for(&self, params: &super::params::TopicParams) -> u32 {
        self.extra
            .iter()
            .filter(|(name, _)| params.is_set(name))
            .fold(self.statements, |sum, (_, extra)| {
                sum.saturating_add(*extra)
            })
    }
}

/// The schema fields an addon contributes to the store schema.
#[derive(Debug, Default)]
pub struct SchemaFragment {
    pub(crate) models: Vec<String>,
    pub(crate) fields: Vec<(String, String, FieldSpec)>,
}

impl SchemaFragment {
    /// Declare a model that has no fields of its own yet (a relation target).
    pub fn model(&mut self, model: &str) -> &mut Self {
        self.models.push(model.to_owned());
        self
    }

    /// Declare one field of a model (the model is declared with it).
    pub fn field(&mut self, model: &str, field: &str, spec: FieldSpec) -> &mut Self {
        self.fields.push((model.to_owned(), field.to_owned(), spec));
        self
    }
}

/// One addon's contribution to the store schema, registered in
/// the `mail.store_schema` slot of `erp_sdk::extensions::EXTENSIONS` through
/// [`store_schema_part!`](crate::store_schema_part). Two parts may declare the same field only identically.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SchemaPart {
    pub(crate) addon: &'static str,
    pub(crate) build: fn(&mut SchemaFragment),
}

impl SchemaPart {
    /// A part of `addon` filled by `build`.
    #[must_use]
    pub const fn new(addon: &'static str, build: fn(&mut SchemaFragment)) -> Self {
        Self { addon, build }
    }

    /// The contributing addon.
    #[must_use]
    pub const fn addon_name(&self) -> &'static str {
        self.addon
    }
}
