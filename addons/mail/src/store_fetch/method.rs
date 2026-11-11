//! The method `mail.store.store_fetch` and the runner behind it.

use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Value as Json, json};
use std::collections::BTreeMap;

use erp_sdk::store_wire::{AnswerBuilder, FieldValue, accept_client_major};
use erp_sdk::{ErrorKind, ValueMap};

use super::MAX_TOPICS_PER_CALL;
use super::params::{self, bounded, to_json};
use super::registry::TopicRegistry;
use super::topic::{Audience, StagedOp, StoreTopic, TopicInput, TopicOutput};

/// The abstract model that hosts `store_fetch`: it has no table and no fields.
#[model(
    name = "mail.store",
    description = "Messaging store",
    kind = "abstract",
    addon = "mail"
)]
pub struct MailStore {}

#[erp_sdk::__private::linkme::distributed_slice(erp_sdk::rpc::PRIMARY_READS)]
#[linkme(crate = erp_sdk::__private::linkme)]
static STORE_FETCH_PRIMARY: erp_sdk::rpc::PrimaryRead = erp_sdk::rpc::PrimaryRead {
    addon: "mail",
    model: "mail.store",
    method: "store_fetch",
};

#[methods(MailStore, addon = "mail")]
impl MailStore {
    /// Run a list of registered topics in this one read-only transaction and answer one store
    /// payload with one as-of, plus one slot per topic..
    ///
    /// # Errors
    /// `Validation` for an unsupported wire major (message starts `client_outdated:`), a malformed,
    /// empty or oversized topic list, an unknown topic or invalid params (before any topic runs);
    /// `Programming` for a topic over its statement budget or a transaction that is not on the
    /// primary at REPEATABLE READ; `Internal` for a topic that stages what the schema refuses; any
    /// other error a topic raises. A denial is a slot, never an error.
    #[method]
    #[rpc(effect = read, model)]
    async fn store_fetch(
        this: &RecordSet<MailStore>,
        tx: &mut Transaction,
        wire: u64,
        topics: Value,
    ) -> Result<Value> {
        let env = this.env().clone();
        run(tx, &env, wire, &topics).await
    }
}

struct Request {
    topic: &'static StoreTopic,
    params: params::TopicParams,
}

struct Item {
    name: String,
    params: Option<Json>,
}

fn parse_items(topics: &Value) -> Result<Vec<Item>> {
    let Value::List(list) = topics else {
        return Err(Error::validation("store_fetch: topics must be a list"));
    };
    if list.is_empty() {
        return Err(Error::validation(
            "store_fetch: topics must name at least one topic",
        ));
    }
    if list.len() > MAX_TOPICS_PER_CALL {
        return Err(Error::validation(format!(
            "store_fetch: {} topics requested; at most {MAX_TOPICS_PER_CALL} are accepted",
            list.len()
        )));
    }
    let mut items = Vec::with_capacity(list.len());
    for (index, raw) in list.iter().enumerate() {
        let Value::Map(map) = raw else {
            return Err(Error::validation(format!(
                "store_fetch: topics[{index}] must be an object with a name"
            )));
        };
        if let Some(key) = map
            .keys()
            .find(|k| !matches!(k.as_str(), "name" | "params"))
        {
            return Err(Error::validation(format!(
                "store_fetch: topics[{index}] has an unknown key '{}'",
                bounded(key)
            )));
        }
        let name = match map.get("name") {
            Some(Value::Str(name)) => name.to_string(),
            Some(_) => {
                return Err(Error::validation(format!(
                    "store_fetch: topics[{index}].name must be text"
                )));
            }
            None => {
                return Err(Error::validation(format!(
                    "store_fetch: topics[{index}] needs a name"
                )));
            }
        };
        let params = map
            .get("params")
            .map(|value| to_json(value, 0, &format!("topics[{index}].params")))
            .transpose()?;
        items.push(Item { name, params });
    }
    Ok(items)
}

fn resolve(registry: &TopicRegistry, env: &Env, items: Vec<Item>) -> Result<Vec<Request>> {
    let mut requests = Vec::with_capacity(items.len());
    for item in items {
        let topic = registry
            .topic(&item.name)
            .filter(|topic| {
                env.registry()
                    .addons()
                    .iter()
                    .any(|addon| addon.name == topic.addon)
            })
            .ok_or_else(|| {
                Error::validation(format!("unknown store topic '{}'", bounded(&item.name)))
            })?;
        let params = params::validate(topic.name, topic.params, item.params.as_ref())?;
        requests.push(Request { topic, params });
    }
    Ok(requests)
}

fn denied(topic: &str, message: &str) -> Json {
    json!({"topic": topic, "ok": false, "error": {"kind": "access", "message": message}})
}

async fn run(tx: &mut Transaction, env: &Env, wire: u64, topics: &Value) -> Result<Value> {
    accept_client_major(wire)
        .map_err(|e| Error::validation(format!("client_outdated: {e}")).with_source(e))?;
    let items = parse_items(topics)?;
    let registry = TopicRegistry::linked()?;
    let requests = resolve(registry, env, items)?;
    let asof = tx.store_snapshot().await?;
    let mut answer = AnswerBuilder::new(registry.schema(), asof);
    let mut slots: Vec<Json> = Vec::with_capacity(requests.len());
    // what earlier topics of this call already put in the payload: two topics may stage the same
    // value for a record (a thread and its messages both name the author); a different value is a defect
    let mut staged: BTreeMap<(String, u64, String), (&'static str, FieldValue)> = BTreeMap::new();
    let mut deleted: BTreeMap<(String, u64), &'static str> = BTreeMap::new();
    // asked at most once per call, and only when a topic needs it
    let mut internal_user: Option<bool> = None;
    for request in &requests {
        let topic = request.topic;
        let allowed = match topic.audience {
            Audience::Internal => match internal_user {
                Some(known) => known,
                None => {
                    let known = tx.has_group(env, "base.group_user").await?;
                    internal_user = Some(known);
                    known
                }
            },
            Audience::LoggedIn => true,
        };
        if !allowed {
            slots.push(denied(
                topic.name,
                &format!(
                    "You are not allowed to fetch the store topic '{}'.",
                    topic.name
                ),
            ));
            continue;
        }
        if topic.needs_groups && internal_user.is_none() {
            // resolves the caller's groups, outside the topic's span (see `needs_groups`)
            internal_user = Some(tx.has_group(env, "base.group_user").await?);
        }
        let input = TopicInput::new(env, &request.params);
        let mut output = TopicOutput::default();
        let before = tx.query_count();
        let mut result = Ok(());
        if let Some(access) = topic.access {
            result = access(tx, &input).await;
        }
        if result.is_ok() {
            result = (topic.run)(tx, &input, &mut output).await;
        }
        let used = tx.query_count() - before;
        // the budget is checked first: a topic that overran it and then raised Access is a defect
        // of the topic (whole call), not a denial
        let budget = topic.budget_for(&request.params);
        if used > u64::from(budget) {
            return Err(Error::programming(format!(
                "store topic '{}' sent {used} statements; its budget is {budget}",
                topic.name
            )));
        }
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::Access => {
                slots.push(denied(topic.name, error.message()));
                continue;
            }
            Err(error) => return Err(error),
        }
        for op in output.ops {
            let applied = match op {
                StagedOp::Set {
                    model,
                    id,
                    field,
                    value,
                } => match staged.get(&(model.clone(), id, field.clone())) {
                    _ if deleted.contains_key(&(model.clone(), id)) => {
                        let owner = deleted[&(model.clone(), id)];
                        return Err(Error::internal(format!(
                            "store topic '{owner}' deletes {model}.{id} but store topic '{}' \
                             sets its field '{field}'",
                            topic.name
                        )));
                    }
                    Some((_, known)) if *known == value => Ok(()),
                    Some((owner, _)) => {
                        return Err(Error::internal(format!(
                            "store topics '{owner}' and '{}' stage different values for \
                             {model}.{id}.{field}",
                            topic.name
                        )));
                    }
                    None => {
                        let done = answer.set(&model, id, &field, value.clone());
                        staged.insert((model, id, field), (topic.name, value));
                        done
                    }
                },
                StagedOp::Delete { model, id } => {
                    let key = (model.clone(), id);
                    if let Some(((_, _, field), (owner, _))) = staged
                        .range((model.clone(), id, String::new())..)
                        .next()
                        .filter(|((m, i, _), _)| *m == model && *i == id)
                    {
                        return Err(Error::internal(format!(
                            "store topic '{}' deletes {model}.{id} but store topic '{owner}' sets \
                             its field '{field}'",
                            topic.name
                        )));
                    }
                    if let std::collections::btree_map::Entry::Vacant(entry) = deleted.entry(key) {
                        entry.insert(topic.name);
                        answer.delete(&model, id)
                    } else {
                        Ok(())
                    }
                }
            };
            applied.map_err(|e| {
                Error::internal(format!(
                    "store topic '{}' produced a record the store schema refuses: {e}",
                    topic.name
                ))
                .with_source(e)
            })?;
        }
        let mut slot = json!({"topic": topic.name, "ok": true});
        if let (Some(data), Some(object)) = (output.data, slot.as_object_mut()) {
            object.insert("data".to_owned(), data);
        }
        slots.push(slot);
    }
    let bytes = answer.encode().map_err(|e| {
        Error::internal(format!("the store answer cannot be encoded: {e}")).with_source(e)
    })?;
    let store: Json = serde_json::from_slice(&bytes)
        .map_err(|e| Error::internal("the encoded store answer is not JSON").with_source(e))?;
    let mut out = ValueMap::default();
    out.insert("store".to_owned(), Value::Json(Arc::new(store)));
    out.insert(
        "topics".to_owned(),
        Value::Json(Arc::new(Json::Array(slots))),
    );
    Ok(Value::Map(Arc::new(out)))
}
