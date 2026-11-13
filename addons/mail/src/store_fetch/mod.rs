//! `store_fetch(topics)`: one closed, batched, read-only fetch of store topics (
//! design).
//!
//! The method [`MailStore::store_fetch`] runs a list of registered [`StoreTopic`]s in one
//! read-only REPEATABLE READ transaction on the primary and answers one store payload with one
//! as-of. An addon adds a topic with the `store_topic!` macro and its schema with a
//! [`SchemaPart`] and the `store_schema_part!` macro; shows how.

mod method;
mod params;
mod registry;
mod topic;
mod topics;

pub use method::MailStore;
pub(crate) use params::bounded;
pub use params::{MAX_PARAMS_BYTES, MAX_PARAMS_DEPTH, ParamDecl, ParamKind, TopicParams};
pub use registry::{Expected, MAIL_EXPECTED, TopicRegistry};
pub use topic::{
    AccessFn, Audience, RunFn, SchemaFragment, SchemaPart, StoreTopic, TopicFuture, TopicInput,
    TopicOutput,
};
pub use topics::SELF_MODEL;

/// Most topics one `store_fetch` call may carry.
pub const MAX_TOPICS_PER_CALL: usize = 16;

/// The extension slot of [`StoreTopic`]s (`erp_sdk::extensions`).
pub const TOPIC_SLOT: &str = "mail.store_topic";
/// The extension slot of [`SchemaPart`]s.
pub const SCHEMA_SLOT: &str = "mail.store_schema";

#[doc(hidden)]
pub use erp_sdk as __sdk;

/// Register a [`StoreTopic`] as a `static` and contribute it to the registry:
/// `store_topic!(MY_TOPIC = StoreTopic::new("my_addon", "my.topic", run));`.
#[macro_export]
macro_rules! store_topic {
    ($(#[$meta:meta])* $vis:vis $name:ident = $topic:expr) => {
        $(#[$meta])*
        $vis static $name: $crate::store_fetch::StoreTopic = $topic;
        const _: () = {
            #[$crate::store_fetch::__sdk::__private::linkme::distributed_slice(
                $crate::store_fetch::__sdk::extensions::EXTENSIONS
            )]
            #[linkme(crate = $crate::store_fetch::__sdk::__private::linkme)]
            static __CONTRIBUTION: $crate::store_fetch::__sdk::extensions::Extension =
                $crate::store_fetch::__sdk::extensions::Extension::new(
                    $crate::store_fetch::TOPIC_SLOT,
                    $name.addon_name(),
                    &$name,
                );
        };
    };
}

/// Register a [`SchemaPart`] as a `static` and contribute it to the store schema.
#[macro_export]
macro_rules! store_schema_part {
    ($(#[$meta:meta])* $vis:vis $name:ident = $part:expr) => {
        $(#[$meta])*
        $vis static $name: $crate::store_fetch::SchemaPart = $part;
        const _: () = {
            #[$crate::store_fetch::__sdk::__private::linkme::distributed_slice(
                $crate::store_fetch::__sdk::extensions::EXTENSIONS
            )]
            #[linkme(crate = $crate::store_fetch::__sdk::__private::linkme)]
            static __CONTRIBUTION: $crate::store_fetch::__sdk::extensions::Extension =
                $crate::store_fetch::__sdk::extensions::Extension::new(
                    $crate::store_fetch::SCHEMA_SLOT,
                    $name.addon_name(),
                    &$name,
                );
        };
    };
}
