//! Administrative detail reads. Durable XML identities, rather than labels, establish ownership.
use super::{CATALOG_SYSTEM, module_error};
use erp_fields::{Error, Value};
use erp_orm::{
    Records, Transaction,
    rpc::{BindError, Effect, ParamSpec, Receiver, RpcArgs, RpcDecl, RpcImpl},
};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

fn uninstall_preview_refusal(blocker: Option<erp_modules::UninstallCompositionBlocker>) -> Error {
    // JSON-2 request owns a transaction and has no trusted session/lock bridge to the engine.
    let message = match blocker {
        Some(blocker) => blocker.message(),
        None => {
            "Native uninstall preview requires a trusted-session engine bridge; unsupported until a later release"
        }
    };
    Error::validation(message)
}

fn digest_hex(input: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(input) {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 15)]));
    }
    text
}

pub(super) static DETAIL_RPC: RpcImpl = RpcImpl {
    addon: "base",
    model: "ir.module.module",
    method: "erp_addon_detail",
    decl: RpcDecl::Expose {
        effect: Effect::Read,
        receiver: Receiver::Model,
        params: &[
            ParamSpec::new("name", true, "string"),
            ParamSpec::new("limit", false, "integer"),
            ParamSpec::new("dependencies_cursor", false, "string"),
            ParamSpec::new("dependents_cursor", false, "string"),
            ParamSpec::new("menus_cursor", false, "string"),
            ParamSpec::new("views_cursor", false, "string"),
        ],
        bind: detail,
    },
    file: file!(),
    line: line!(),
};

pub(super) static PREVIEW_RPC: RpcImpl = RpcImpl {
    addon: "base",
    model: "ir.module.module",
    method: "erp_preview_uninstall",
    decl: RpcDecl::Expose {
        effect: Effect::Read,
        receiver: Receiver::Model,
        params: &[ParamSpec::new("modules", true, "array of strings")],
        bind: preview,
    },
    file: file!(),
    line: line!(),
};

pub(super) fn modules(names: Vec<String>) -> Result<Vec<erp_modules::ModuleName>, Error> {
    if names.is_empty() || names.len() > 64 {
        return Err(Error::validation(
            "Module names must contain between 1 and 64 names",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    names
        .into_iter()
        .map(|name| {
            if name.len() > 256 {
                return Err(Error::validation("Addon name exceeds 256 bytes"));
            }
            if !seen.insert(name.clone()) {
                return Err(Error::validation("Duplicate addon name"));
            }
            erp_modules::ModuleName::new(name).map_err(module_error)
        })
        .collect()
}

fn preview<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> erp_orm::BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        authorize(tx, &records).await?;
        modules(args.take("modules")?)?;
        if args.remaining().next().is_some() {
            return Err(Error::validation("Unknown uninstall preview argument").into());
        }
        let system = CATALOG_SYSTEM.get().ok_or_else(|| {
            Error::programming("The uninstall preview is not bound to a module system")
        })?;
        Err(uninstall_preview_refusal(system.uninstall_composition_blocker()).into())
    })
}

pub(super) async fn authorize(tx: &mut Transaction, records: &Records) -> Result<(), Error> {
    let principal = records.env().principal();
    if tx.route() != erp_orm::Route::Primary
        || tx.env().principal() != principal
        || principal.kind() != erp_orm::PrincipalKind::User
        || principal.actor().is_some()
        || principal.scope().is_some()
        || !matches!(principal.elevation(), erp_orm::Elevation::None)
        || !principal.resolution().is_resolved()
    {
        return Err(Error::access(
            "Addon administration requires a resolved, unelevated primary session user",
        ));
    }
    if !tx.has_group(records.env(), "base.group_system").await? {
        return Err(Error::access(
            "Addon administration requires system administration authority",
        ));
    }
    records.check_access(tx, erp_orm::Operation::Read).await
}

fn page(
    items: &[Json],
    name: &str,
    collection: &str,
    revision: &str,
    cursor: Option<String>,
    limit: usize,
) -> Result<Json, Error> {
    let binding = digest_hex(format!("{name}\0{collection}\0{revision}"));
    let after = match cursor {
        None => None,
        Some(cursor) => {
            if cursor.len() > 512 {
                return Err(Error::validation("Addon detail cursor exceeds its bound"));
            }
            let mut parts = cursor.splitn(3, ':');
            if parts.next() != Some("detail-v1") || parts.next() != Some(binding.as_str()) {
                return Err(Error::validation(
                    "Addon detail cursor is malformed or stale",
                ));
            }
            Some(
                parts
                    .next()
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| Error::validation("Addon detail cursor is malformed or stale"))?
                    .to_owned(),
            )
        }
    };
    let key = |value: &Json| -> Result<String, Error> {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                value
                    .get("id")
                    .and_then(Json::as_u64)
                    .map(|id| format!("{id:016}"))
            })
            .ok_or_else(|| Error::programming("Invalid addon detail page identity"))
    };
    let mut keyed = items
        .iter()
        .map(|item| Ok((key(item)?, item)))
        .collect::<Result<Vec<_>, Error>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(after) = &after
        && !keyed.iter().any(|(key, _)| key == after)
    {
        return Err(Error::validation(
            "Addon detail cursor is malformed or stale",
        ));
    }
    let remaining = keyed
        .into_iter()
        .filter(|(key, _)| after.as_ref().is_none_or(|after| key > after))
        .collect::<Vec<_>>();
    let next_cursor = if remaining.len() > limit {
        Some(format!("detail-v1:{binding}:{}", remaining[limit - 1].0))
    } else {
        None
    };
    Ok(
        json!({"items":remaining.iter().take(limit).map(|(_,item)| *item).collect::<Vec<_>>(), "next_cursor":next_cursor}),
    )
}

fn optional<T: erp_orm::rpc::FromRpc>(
    args: &mut RpcArgs,
    name: &'static str,
) -> Result<Option<T>, BindError> {
    if args.remaining().any(|key| key == name) {
        args.take(name).map(Some)
    } else {
        Ok(None)
    }
}

fn detail<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> erp_orm::BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        authorize(tx, &records).await?;
        let name: String = args.take("name")?;
        if name.len() > 256 {
            return Err(Error::validation("Addon name exceeds 256 bytes").into());
        }
        erp_modules::ModuleName::new(name.clone()).map_err(module_error)?;
        let limit = optional::<u64>(&mut args, "limit")?.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(Error::validation("Addon detail limit must be between 1 and 100").into());
        }
        let cursors = [
            optional::<String>(&mut args, "dependencies_cursor")?,
            optional(&mut args, "dependents_cursor")?,
            optional(&mut args, "menus_cursor")?,
            optional(&mut args, "views_cursor")?,
        ];
        if args.remaining().next().is_some() {
            return Err(Error::validation("Unknown addon detail argument").into());
        }
        let system = CATALOG_SYSTEM.get().ok_or_else(|| {
            Error::programming("The addon detail is not bound to a module system")
        })?;
        let catalog = system.human_catalog().await.map_err(module_error)?;
        let manifest = catalog.offered.iter().find(|m| m.name().as_str() == name);
        if let Some(manifest) = manifest {
            let text_bytes = [
                manifest.shortdesc(),
                manifest.summary(),
                manifest.description(),
                manifest.author(),
            ]
            .iter()
            .try_fold(0usize, |sum, text| sum.checked_add(text.len()))
            .ok_or_else(|| {
                Error::validation("Addon detail manifest exceeds its admission bound")
            })?;
            if text_bytes > 65536 {
                return Err(
                    Error::validation("Addon detail manifest exceeds its admission bound").into(),
                );
            }
        }
        // One statement gives the revision and every collection the same MVCC snapshot. Each
        // individual text and the aggregate snapshot are admitted before returning JSON bytes.
        let rows = tx.query(r#"
WITH owned AS (SELECT model,res_id FROM ir_model_data WHERE module=$1),
entries AS (SELECT * FROM (
 SELECT 'menus'::text AS collection, CASE WHEN octet_length(m.name)<=4096 THEN jsonb_build_object('id',m.id,'name',m.name,'label',m.name) ELSE NULL END AS item
 FROM ir_ui_menu m WHERE EXISTS(SELECT 1 FROM owned WHERE model='ir.ui.menu' AND res_id=m.id)
 UNION ALL
 SELECT 'views',CASE WHEN octet_length(v.name)<=4096 AND octet_length(v.model)<=4096 AND octet_length(v.type)<=256 THEN jsonb_build_object('id',v.id,'name',v.name,'model',v.model,'kind',v.type) ELSE NULL END
 FROM ir_ui_view v WHERE EXISTS(SELECT 1 FROM owned WHERE model='ir.ui.view' AND res_id=v.id)
 UNION ALL
 SELECT 'dependencies',CASE WHEN octet_length(d.name)<=256 THEN to_jsonb(d.name::text) ELSE NULL END FROM ir_module_module_dependency d JOIN ir_module_module m ON m.id=d.module_id WHERE m.name=$1
 UNION ALL
 SELECT 'dependents',CASE WHEN octet_length(m.name)<=256 THEN to_jsonb(m.name::text) ELSE NULL END FROM ir_module_module_dependency d JOIN ir_module_module m ON m.id=d.module_id WHERE d.name=$1 AND m.state <> 'uninstalled'
) bounded LIMIT 10001), bounds AS (SELECT count(*) AS count,count(item) AS admitted,COALESCE(sum(octet_length(item::text)),0) AS bytes FROM entries),
stored AS (SELECT CASE WHEN COALESCE(octet_length(shortdesc)::bigint,0)+COALESCE(octet_length(summary)::bigint,0)+COALESCE(octet_length(description)::bigint,0)+COALESCE(octet_length(author)::bigint,0)<=65536 AND COALESCE(octet_length(state),0)<=64 AND COALESCE(octet_length(latest_version),0)<=256 THEN jsonb_build_object('shortdesc',shortdesc,'summary',summary,'description',description,'author',author,'state',state,'latest_version',latest_version) ELSE NULL END AS item FROM ir_module_module WHERE name=$1)
SELECT CASE WHEN bounds.count<=10000 AND bounds.count=bounds.admitted AND bounds.bytes<=1048576 AND NOT EXISTS(SELECT 1 FROM stored WHERE item IS NULL)
 THEN jsonb_build_object('stored',(SELECT item FROM stored),'entries',COALESCE((SELECT jsonb_agg(jsonb_build_object('collection',collection,'item',item) ORDER BY collection,item::text) FROM entries),'[]'::jsonb)) ELSE NULL END FROM bounds
"#, &[&name]).await?;
        let [row] = rows.as_slice() else {
            return Err(Error::programming("Invalid addon detail snapshot cardinality").into());
        };
        let snapshot: Option<Json> = row
            .try_get(0)
            .map_err(|e| Error::db("Invalid addon detail snapshot").with_source(e))?;
        let snapshot = snapshot.ok_or_else(|| {
            Error::validation("Addon detail metadata exceeds its admission bound")
        })?;
        let stored = &snapshot["stored"];
        if stored.is_null() && manifest.is_none() {
            return Err(Error::validation("Unknown addon name").into());
        }
        if stored["state"] == "uninstalled" && !stored["latest_version"].is_null() {
            return Err(
                Error::programming("Uninstalled addon retains an installed version").into(),
            );
        }
        let text = |key: &str| -> Result<&str, Error> {
            match &stored[key] {
                Json::String(value) => Ok(value),
                // Optional persisted descriptions may be absent; their required wire text is empty.
                Json::Null if matches!(key, "summary" | "description" | "author") => Ok(""),
                _ => Err(Error::programming(format!(
                    "Invalid stored addon detail field {key}"
                ))),
            }
        };
        let mut collections: [Vec<Json>; 4] = Default::default();
        for entry in snapshot["entries"]
            .as_array()
            .ok_or_else(|| Error::programming("Invalid addon detail collections"))?
        {
            let index = match entry["collection"].as_str() {
                Some("dependencies") => 0,
                Some("dependents") => 1,
                Some("menus") => 2,
                Some("views") => 3,
                _ => return Err(Error::programming("Unknown addon detail collection").into()),
            };
            collections[index].push(entry["item"].clone());
        }
        if let Some(manifest) = manifest {
            if manifest.depends().len() > 10000 {
                return Err(
                    Error::validation("Addon detail metadata exceeds its admission bound").into(),
                );
            }
            collections[0] = manifest
                .depends()
                .iter()
                .map(|n| json!(n.as_str()))
                .collect();
        }
        for candidate in &catalog.offered {
            if candidate.depends().iter().any(|n| n.as_str() == name) {
                if collections[1].len() >= 10000 {
                    return Err(Error::validation(
                        "Addon detail metadata exceeds its admission bound",
                    )
                    .into());
                }
                collections[1].push(json!(candidate.name().as_str()));
            }
        }
        for collection in &mut collections {
            collection.sort_by_key(Json::to_string);
            collection.dedup();
        }
        let state = if stored.is_null() {
            "uninstalled"
        } else {
            stored["state"]
                .as_str()
                .ok_or_else(|| Error::programming("Invalid stored addon lifecycle state"))?
        };
        let state = match state {
            "to install" => "to_install",
            "to upgrade" => "to_upgrade",
            "to remove" => "to_remove",
            "installed" | "uninstalled" | "uninstallable" => state,
            _ => return Err(Error::programming("Unknown stored addon lifecycle state").into()),
        };
        let stable = matches!(state, "installed" | "uninstalled");
        let upgrade_refusal = if !stable {
            Some("Pending module operations require operator recovery")
        } else if state != "installed" {
            Some("Addon is not installed")
        } else if manifest.is_none() {
            Some("Installed addon source is unavailable to native upgrade")
        } else {
            None
        };
        let uninstall_refusal = uninstall_preview_refusal(system.uninstall_composition_blocker());
        let mut document = json!({"wire_major":1,"registry_version":records.env().registry().generation().to_string(),"name":name,
            "label":match manifest {Some(m)=>m.shortdesc(),None=>text("shortdesc")?},
            "summary":match manifest {Some(m)=>m.summary(),None=>text("summary")?},
            "description":match manifest {Some(m)=>m.description(),None=>text("description")?},
            "author":match manifest {Some(m)=>m.author(),None=>text("author")?},
            "available_version":manifest.map(|m|m.version().to_string()),"installed_version":stored["latest_version"],"state":state,
            "upgrade_refusal":upgrade_refusal,"uninstall_refusal":uninstall_refusal.message()});
        let revision_bytes = serde_json::to_vec(&(&document, &collections)).map_err(|e| {
            Error::programming("Addon detail revision serialization failed").with_source(e)
        })?;
        if revision_bytes.len() > 1048576 {
            return Err(
                Error::validation("Addon detail metadata exceeds its admission bound").into(),
            );
        }
        let revision = digest_hex(revision_bytes);
        for ((index, collection), cursor) in ["dependencies", "dependents", "menus", "views"]
            .into_iter()
            .enumerate()
            .zip(cursors)
        {
            document[collection] = page(
                &collections[index],
                &name,
                collection,
                &revision,
                cursor,
                limit as usize,
            )?;
        }
        let bytes = serde_json::to_vec(&document)
            .map_err(|e| Error::programming("Addon detail serialization failed").with_source(e))?;
        if bytes.len() > 65536 {
            return Err(Error::validation("Addon detail response exceeds 65536 bytes").into());
        }
        let checked: erp_views::wire::AddonDetail =
            serde_json::from_value(document).map_err(|e| {
                Error::programming("Addon detail failed its wire contract").with_source(e)
            })?;
        Ok(Value::Json(Arc::new(
            serde_json::to_value(checked).map_err(|e| {
                Error::programming("Addon detail serialization failed").with_source(e)
            })?,
        )))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit test assertions")]
mod tests {
    use super::*;
    #[test]
    fn absent_composition_blocker_does_not_advertise_uninstall_support() {
        let error = uninstall_preview_refusal(None);
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Native uninstall preview requires a trusted-session engine bridge; unsupported until a later release",
        );
    }

    #[test]
    fn preview_refusal_retains_engine_composition_reason() {
        let error = uninstall_preview_refusal(Some(
            erp_modules::UninstallCompositionBlocker::ConfiguredSignal,
        ));
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Native lifecycle opening with configured Signal is unsupported until a later release",
        );
    }

    fn rejection(result: Result<Json, Error>) {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(error.message(), "Addon detail cursor is malformed or stale");
    }
    #[test]
    fn detail_cursor_binds_addon_collection_and_authoritative_revision() {
        let items = vec![
            json!({"id":10,"name":"ten","label":"ten"}),
            json!({"id":2,"name":"two","label":"two"}),
        ];
        let first = page(&items, "sample", "menus", "revision-1", None, 1).unwrap();
        assert_eq!(first["items"][0]["id"], 2);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let second = page(
            &items,
            "sample",
            "menus",
            "revision-1",
            Some(cursor.clone()),
            1,
        )
        .unwrap();
        assert_eq!(second["items"][0]["id"], 10);
        assert!(second["next_cursor"].is_null());
        rejection(page(
            &items,
            "other",
            "menus",
            "revision-1",
            Some(cursor.clone()),
            1,
        ));
        rejection(page(
            &items,
            "sample",
            "views",
            "revision-1",
            Some(cursor.clone()),
            1,
        ));
        rejection(page(
            &items,
            "sample",
            "menus",
            "revision-2",
            Some(cursor),
            1,
        ));
        rejection(page(
            &items,
            "sample",
            "menus",
            "revision-1",
            Some("forged".to_owned()),
            1,
        ));
        let empty = page(&[], "sample", "menus", "revision-1", None, 50).unwrap();
        assert_eq!(empty, json!({"items":[],"next_cursor":null}));
    }
}
