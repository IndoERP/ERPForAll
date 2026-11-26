// SPDX-License-Identifier: MIT OR Apache-2.0
//! Principal-filtered assistant discovery. Execution shares the JSON-2 RPC transaction.
use super::*;
use erp_orm::{Operation, rpc::Offered};
use erp_registry::tool_schema::{ToolSchema, params_schema};

pub(super) fn offered<'a>(orm: &'a Orm, route: &Route) -> Option<&'a Offered> {
    let model = orm.registry().model_id(route.model())?;
    orm.exposure().assistant_offered(model, route.method()).ok()
}

pub(super) fn input_schema(tool: &Offered) -> Result<ToolSchema> {
    let mut schema = params_schema(tool.model(), tool.method(), tool.params())
        .map_err(|error| Error::programming(error.to_string()))?;
    if tool.receiver() == Receiver::Records {
        let ToolSchema::Object(object) = &mut schema else {
            return Err(Error::programming("Tool schema must be an object."));
        };
        let ids = ToolSchema::Array {
            items: Box::new(ToolSchema::Integer {
                minimum: Some(1),
                maximum: None,
            }),
            min_items: None,
            max_items: None,
        };
        if !object.insert("ids", ids, Some("Record IDs"), false) {
            return Err(Error::programming(
                "Offered method has a reserved ids parameter.",
            ));
        }
    }
    Ok(schema)
}

impl RpcService {
    pub(super) async fn assistant_access(
        &self,
        orm: &Orm,
        tx: &mut Transaction,
        route: &Route,
        ids: &[i64],
        key: Option<&IdempotencyKey>,
    ) -> Result<bool> {
        let Some(model) = orm.registry().model_id(route.model()) else {
            return Ok(false);
        };
        let Ok(tool) = orm.exposure().assistant_offered(model, route.method()) else {
            return Ok(false);
        };
        let env = tx.env();
        let principal = env.principal();
        if !matches!(principal.kind(), PrincipalKind::Agent | PrincipalKind::User)
            || crate::PasswordOperation::lookup(tool.model(), tool.method()).is_some()
        {
            return Ok(false);
        }
        if principal.kind() == PrincipalKind::Agent && self.attempt_gate.is_none() {
            return Ok(false);
        }
        if let Some(scope) = principal.scope()
            && (!scope.models().allows(tool.model())
                || !scope.methods().allows(tool.method())
                || (scope.read_only() && tool.effect() != Effect::Read))
        {
            return Ok(false);
        }
        match self.gates_check(route, principal, tool.effect(), key) {
            Ok(()) => {}
            Err(refusal) if matches!(refusal.status(), 401 | 403 | 404) => return Ok(false),
            Err(refusal) => {
                return Err(Error::internal(format!(
                    "Tool gate failed (HTTP {}).",
                    refusal.status()
                )));
            }
        }
        let operation = match tool.effect() {
            Effect::Read => Operation::Read,
            Effect::Write => Operation::Write,
            Effect::Destructive => Operation::Unlink,
            Effect::External => return Ok(false),
        };
        tx.env()
            .records(tool.model())?
            .browse(ids.iter().copied())?
            .has_access(tx, operation)
            .await
    }

    /// Build a bounded page from the active registry and a freshly authenticated caller transaction.
    /// The cursor is the last returned qualified tool name.
    /// # Errors
    /// Returns a database or schema error when discovery cannot establish the caller's authority.
    pub async fn assistant_tools(
        &self,
        orm: &Orm,
        tx: &mut Transaction,
        after: Option<&str>,
    ) -> Result<(Vec<Json>, Option<String>)> {
        if !Arc::ptr_eq(orm.registry(), tx.registry()) {
            return Err(Error::access("Tool registry changed; repeat discovery."));
        }
        const PAGE: usize = 64;
        let mut tools = Vec::new();
        let mut previous = None;
        let mut offers = orm
            .exposure()
            .assistant_offers()
            .into_iter()
            .map(|tool| (format!("{}.{}", tool.model(), tool.method()), tool))
            .collect::<Vec<_>>();
        offers.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for (name, tool) in offers {
            if name.len() > 128 || after.is_some_and(|after| name.as_str() <= after) {
                continue;
            }
            let route = Route::parse_json2(&format!("/json/2/{}/{}", tool.model(), tool.method()))
                .map_err(|_| Error::programming("Invalid offered tool route."))?;
            if !self.assistant_access(orm, tx, &route, &[], None).await? {
                continue;
            }
            if let Some(gate) = &self.attempt_gate
                && !gate
                    .assistant_visible(tx, tool.model(), tool.method(), tool.effect())
                    .await?
            {
                continue;
            }
            if tools.len() == PAGE {
                return Ok((tools, previous));
            }
            let input = input_schema(tool)?.to_json();
            previous = Some(name.clone());
            tools.push(Json::Object(vec![
                ("name".into(), Json::Str(name.into())),
                ("inputSchema".into(), input),
                (
                    "annotations".into(),
                    Json::Object(vec![
                        (
                            "readOnlyHint".into(),
                            Json::Bool(tool.effect().read_only_hint()),
                        ),
                        (
                            "destructiveHint".into(),
                            Json::Bool(tool.effect().destructive_hint()),
                        ),
                        ("openWorldHint".into(), Json::Bool(false)),
                    ]),
                ),
            ]));
        }
        Ok((tools, None))
    }
}
