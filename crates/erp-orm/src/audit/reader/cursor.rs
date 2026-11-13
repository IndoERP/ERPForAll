// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{
    invalid,
    types::{AuditQuery, EntryKey, object},
};
use crate::{Env, Transaction};
use erp_fields::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) async fn binding(
    tx: &mut Transaction,
    env: &Env,
    query: &AuditQuery,
    database: &str,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"erp-audit-reader-v1\0");
    let mut put = |b: &[u8]| {
        hash.update(b.len().to_le_bytes());
        hash.update(b);
    };
    put(database.as_bytes());
    put(&env.principal().binding_bytes());
    put(query.canonical().to_string().as_bytes());
    // Registry::generation is process-local. Bind the database's global generations,
    // including membership, in this primary snapshot so another node can accept the page.
    let rows = tx.query("SELECT key, sum(weight)::int8 FROM erp_signal_generation WHERE key IN ('registry','policy','membership') GROUP BY key", &[])
        .await.map_err(super::access::sanitize)?;
    // Signal::Generations specifies generation zero when a key has no rows.
    let mut generations = [0_u64; 3];
    for row in rows {
        let key: &str = row.try_get(0).map_err(|_| super::integrity())?;
        let value: i64 = row.try_get(1).map_err(|_| super::integrity())?;
        let slot = match key {
            "registry" => 0,
            "policy" => 1,
            "membership" => 2,
            _ => return Err(super::integrity()),
        };
        generations[slot] = u64::try_from(value).map_err(|_| super::integrity())?;
    }
    for value in generations {
        put(&value.to_le_bytes());
    }
    let b = tx.binding(env)?;
    for group in b.resolution().groups().ids() {
        put(&group.get().to_le_bytes());
    }
    for company in tx.companies_now(env)? {
        put(&company.get().to_le_bytes());
    }
    // Field-context values are hashed within the admitted build; no context values or hidden
    // row data enter the continuation. Shared-build admission belongs to INTEGRATE.
    for key in tx.policy().field_context_keys() {
        put(key.as_bytes());
        let value = env.context().get(key);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&value, &mut h);
        put(&std::hash::Hasher::finish(&h).to_le_bytes());
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
pub(super) fn encode(key: &EntryKey, binding: &str) -> String {
    json!({"v":1,"key":key.to_json(),"binding":binding}).to_string()
}
pub(super) fn decode(token: &str, binding: &str) -> Result<EntryKey> {
    let bad = || invalid("invalid continuation");
    if token.len() > 1024 {
        return Err(bad());
    }
    let v: Value = serde_json::from_str(token).map_err(|_| bad())?;
    let map = object(&v, &["v", "key", "binding"], "invalid continuation")?;
    if map.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(bad());
    }
    if map.get("binding").and_then(Value::as_str) != Some(binding) {
        return Err(invalid("stale or mismatched continuation"));
    }
    EntryKey::parse(map.get("key").ok_or_else(bad)?).map_err(|_| bad())
}
