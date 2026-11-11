//! The closed topic registry and the store schema it derives (2, D2.3).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use erp_sdk::serde_json::{self, Value as Json};
use erp_sdk::store_wire::{FieldSpec, SchemaGeneration, StoreSchema};
use erp_sdk::{Error, Result};

use super::topic::{SchemaFragment, SchemaPart, StoreTopic};
use super::{SCHEMA_SLOT, TOPIC_SLOT};

/// What an addon's build site requires of the link-time slices: a registration the
/// linker dropped is an error naming the owner, not a missing topic at the first call.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Expected {
    addon: &'static str,
    topics: &'static [&'static str],
    schema_part: bool,
}

impl Expected {
    /// Topics `topics` of `addon`, and a schema part of the addon when `schema_part`.
    #[must_use]
    pub const fn new(
        addon: &'static str,
        topics: &'static [&'static str],
        schema_part: bool,
    ) -> Self {
        Self {
            addon,
            topics,
            schema_part,
        }
    }
}

/// What the `mail` addon itself registers.
pub const MAIL_EXPECTED: Expected = Expected::new(
    "mail",
    &[
        "schema",
        "messaging.self",
        "thread",
        "thread.messages",
        "thread.scheduled",
    ],
    true,
);

const MAX_NAME_BYTES: usize = 64;

/// The topics, and the schema built from the addons' parts.
#[derive(Debug)]
pub struct TopicRegistry {
    topics: BTreeMap<&'static str, &'static StoreTopic>,
    schema: StoreSchema,
    schema_document: Json,
}

fn valid_topic_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.'))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn wire_err(error: erp_sdk::store_wire::WireError) -> Error {
    Error::programming(format!("the store schema is invalid: {error}")).with_source(error)
}

impl TopicRegistry {
    /// Build a registry from explicit parts. The linked registry ([`TopicRegistry::linked`]) calls it
    /// with the slices; an addon's build site or test calls it with its own.
    ///
    /// # Errors
    /// `Programming` for a duplicate or ill-formed topic name, a duplicate parameter name, a
    /// `statements_plus` entry that names no declared optional parameter, repeats one or adds 0,
    /// conflicting or invalid schema declarations, and an expected topic or schema part that is
    /// absent (naming the entry and the owner crate `erp-addon-<addon>`).
    pub fn from_parts(
        topics: &[&'static StoreTopic],
        parts: &[&'static SchemaPart],
        expected: &[Expected],
    ) -> Result<Self> {
        let mut by_name: BTreeMap<&'static str, &'static StoreTopic> = BTreeMap::new();
        for topic in topics {
            if !valid_topic_name(topic.name) {
                return Err(Error::programming(format!(
                    "store topic name '{}' of addon '{}' is invalid: use 1..={MAX_NAME_BYTES} bytes \
                     of a-z, 0-9, '_' and '.', starting with a letter",
                    super::params::bounded(topic.name),
                    topic.addon
                )));
            }
            let mut seen = BTreeSet::new();
            for decl in topic.params {
                if !seen.insert(decl.name()) {
                    return Err(Error::programming(format!(
                        "store topic '{}' of addon '{}' declares param '{}' twice",
                        topic.name,
                        topic.addon,
                        decl.name()
                    )));
                }
            }
            let mut raised = BTreeSet::new();
            for (param, extra) in topic.extra {
                if !topic
                    .params
                    .iter()
                    .any(|decl| decl.name() == *param && !decl.is_required())
                {
                    return Err(Error::programming(format!(
                        "store topic '{}' of addon '{}' raises its budget for param '{param}', \
                         which is not a declared optional param",
                        topic.name, topic.addon
                    )));
                }
                if !raised.insert(*param) {
                    return Err(Error::programming(format!(
                        "store topic '{}' of addon '{}' raises its budget for param '{param}' twice",
                        topic.name, topic.addon
                    )));
                }
                if *extra == 0 {
                    return Err(Error::programming(format!(
                        "store topic '{}' of addon '{}' raises its budget for param '{param}' by 0 \
                         statements: drop the entry",
                        topic.name, topic.addon
                    )));
                }
            }
            if let Some(first) = by_name.insert(topic.name, topic) {
                return Err(Error::programming(format!(
                    "store topic '{}' is registered twice: by addon '{}' and by addon '{}'",
                    topic.name, first.addon, topic.addon
                )));
            }
        }
        for want in expected {
            for name in want.topics {
                if !by_name.get(name).is_some_and(|t| t.addon == want.addon) {
                    return Err(Error::programming(format!(
                        "store topic '{name}' of addon '{}' is expected but not linked: its \
                         registration was dropped from the build (crate erp-addon-{})",
                        want.addon, want.addon
                    )));
                }
            }
            if want.schema_part && !parts.iter().any(|p| p.addon == want.addon) {
                return Err(Error::programming(format!(
                    "the store schema part of addon '{}' is expected but not linked: its \
                     registration was dropped from the build (crate erp-addon-{})",
                    want.addon, want.addon
                )));
            }
        }
        let (schema, schema_document) = build_schema(parts)?;
        Ok(Self {
            topics: by_name,
            schema,
            schema_document,
        })
    }

    /// Build a registry from contributions that carry the addon they were registered under (the
    /// `Extension` of the slot). A contribution whose registered addon differs from the addon the
    /// item names is refused ((a).4): one item must not carry two names.
    ///
    /// # Errors
    /// `Programming` for a mismatch, and the errors of [`TopicRegistry::from_parts`].
    pub fn from_contributions(
        topics: &[(&'static str, &'static StoreTopic)],
        parts: &[(&'static str, &'static SchemaPart)],
        expected: &[Expected],
    ) -> Result<Self> {
        for (registered, topic) in topics {
            if *registered != topic.addon {
                return Err(Error::programming(format!(
                    "store topic '{}' is contributed by addon '{registered}' but names addon '{}'",
                    super::params::bounded(topic.name),
                    topic.addon
                )));
            }
        }
        for (registered, part) in parts {
            if *registered != part.addon {
                return Err(Error::programming(format!(
                    "a store schema part is contributed by addon '{registered}' but names addon '{}'",
                    part.addon
                )));
            }
        }
        let topics: Vec<&'static StoreTopic> = topics.iter().map(|(_, t)| *t).collect();
        let parts: Vec<&'static SchemaPart> = parts.iter().map(|(_, p)| *p).collect();
        Self::from_parts(&topics, &parts, expected)
    }

    #[allow(clippy::type_complexity, reason = "two lists of (addon, item)")]
    fn collect() -> Result<(
        Vec<(&'static str, &'static StoreTopic)>,
        Vec<(&'static str, &'static SchemaPart)>,
    )> {
        let topics = erp_sdk::extensions::items::<StoreTopic>(TOPIC_SLOT)?;
        let parts = erp_sdk::extensions::items::<SchemaPart>(SCHEMA_SLOT)?;
        // Until installed-only schema filtering exists the `schema` topic would expose every linked
        // part to every database, so only `mail` may contribute ((c).4).
        if let Some((addon, _)) = parts.iter().find(|(addon, _)| *addon != "mail") {
            return Err(Error::programming(format!(
                "store schema part of addon '{addon}' is not supported until \
                 a later release: the schema topic would show it in databases where \
                 the addon is not installed"
            )));
        }
        Ok((topics, parts))
    }

    /// The registry linked into this process, built once with the `mail` addon's expected set.
    /// A build failure is returned by every call.
    ///
    /// # Errors
    /// The error of [`TopicRegistry::from_contributions`], or the refusal of a schema part of an
    /// addon other than `mail`.
    pub fn linked() -> Result<&'static Self> {
        static LINKED: LazyLock<std::result::Result<TopicRegistry, String>> = LazyLock::new(|| {
            let build = || -> Result<TopicRegistry> {
                let (topics, parts) = TopicRegistry::collect()?;
                TopicRegistry::from_contributions(&topics, &parts, &[MAIL_EXPECTED])
            };
            build().map_err(|e| e.message().to_owned())
        });
        LINKED
            .as_ref()
            .map_err(|message| Error::programming(message.clone()))
    }

    /// The startup check of a link root ((d)): the linked registry builds, and
    /// the topics of every `extra` expectation (an addon that registers topics passes its own) are
    /// present. A link root calls this before serving, so a registration the linker dropped stops
    /// the start instead of reading as `unknown store topic` to a client.
    ///
    /// # Errors
    /// `Programming` naming the missing entry and the owner crate.
    pub fn require_linked(extra: &[Expected]) -> Result<()> {
        Self::linked()?;
        let (topics, parts) = Self::collect()?;
        let mut expected = vec![MAIL_EXPECTED];
        expected.extend_from_slice(extra);
        Self::from_contributions(&topics, &parts, &expected).map(drop)
    }

    /// The topic named `name`, if registered.
    #[must_use]
    pub fn topic(&self, name: &str) -> Option<&'static StoreTopic> {
        self.topics.get(name).copied()
    }

    /// The registered topic names, sorted.
    pub fn topic_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.topics.keys().copied()
    }

    /// The store schema of every part.
    #[must_use]
    pub fn schema(&self) -> &StoreSchema {
        &self.schema
    }

    /// The schema as the JSON document the `schema` topic returns.
    #[must_use]
    pub fn schema_document(&self) -> &Json {
        &self.schema_document
    }
}

fn build_schema(parts: &[&'static SchemaPart]) -> Result<(StoreSchema, Json)> {
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut fields: BTreeMap<(String, String), (&'static str, FieldSpec)> = BTreeMap::new();
    for part in parts {
        let mut fragment = SchemaFragment::default();
        (part.build)(&mut fragment);
        models.extend(fragment.models);
        for (model, field, spec) in fragment.fields {
            models.insert(model.clone());
            match fields.get(&(model.clone(), field.clone())) {
                Some((_, known)) if *known == spec => {}
                Some((owner, _)) => {
                    return Err(Error::programming(format!(
                        "store schema field {model}.{field} is declared differently by addon \
                         '{owner}' and addon '{}'",
                        part.addon
                    )));
                }
                None => {
                    fields.insert((model, field), (part.addon, spec));
                }
            }
        }
    }
    let make = |token: &str| -> Result<StoreSchema> {
        let mut builder = StoreSchema::builder(SchemaGeneration::new(token).map_err(wire_err)?);
        for model in &models {
            builder.model(model).map_err(wire_err)?;
        }
        for ((model, field), (_, spec)) in &fields {
            builder
                .field(model, field, spec.clone())
                .map_err(wire_err)?;
        }
        builder.build().map_err(wire_err)
    };
    let pending = make("pending")?.encode().map_err(wire_err)?;
    let token = format!("s1-{:016x}", fnv1a64(&pending));
    let schema = make(&token)?;
    let document = serde_json::from_slice(&schema.encode().map_err(wire_err)?)
        .map_err(|e| Error::internal("the encoded schema is not JSON").with_source(e))?;
    Ok((schema, document))
}
