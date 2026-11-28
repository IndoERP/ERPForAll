// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reusable report definitions. Recipient-specific values are computed when the report opens.
use crate::aggregate;
use erp_sdk::ai::{ToolCall, ToolCallId};
use erp_sdk::client_actions::ClientActionDecl;
use erp_sdk::domain::Value as Operand;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

const TAG: &str = "ai_base.saved_insight";
const MAX_PANELS: usize = 8;
const MAX_DEFINITION_BYTES: usize = 32 * 1024;

#[model(name="ai.insight", description="Saved Insight", order="name, id",
    company_access=direct(field="company_id",branch="exact",empty="deny"))]
pub struct Insight {
    #[field(char, required = true, size = 160)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "res.groups",
        index = true,
        ondelete = "set null",
        string = "Share with group"
    )]
    pub shared_group_id: Option<RecId>,
    #[field(text, compute = "_compute_sources", string = "Data sources")]
    pub sources: Option<Arc<str>>,
    #[field(text, required = true, string = "Query definition")]
    pub definition: Option<Arc<str>>,
    #[field(
        boolean,
        compute = "_compute_group_member",
        search_method = "_search_group_member"
    )]
    pub group_member: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Visualization {
    #[default]
    Table,
    Bar,
    Line,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Panel {
    title: String,
    #[serde(default)]
    visualization: Visualization,
    model: String,
    domain: Vec<Json>,
    groupby: Vec<String>,
    aggregates: Vec<String>,
    order: String,
    limit: usize,
}

enum Definition {
    Report(i64),
    Dashboard(Vec<Panel>),
}

fn parse(raw: &str) -> Result<Definition> {
    let invalid = || Error::validation("Use a report_id or 1–8 live aggregation panels.");
    if raw.len() > MAX_DEFINITION_BYTES {
        return Err(invalid());
    }
    let parsed: Map<String, Json> = serde_json::from_str(raw).map_err(|_| invalid())?;
    if parsed.len() != 1 {
        return Err(invalid());
    }
    if let Some(id) = parsed
        .get("report_id")
        .and_then(Json::as_i64)
        .filter(|id| *id > 0)
    {
        return Ok(Definition::Report(id));
    }
    let panels = parsed
        .get("panels")
        .and_then(Json::as_array)
        .filter(|panels| (1..=MAX_PANELS).contains(&panels.len()))
        .ok_or_else(invalid)?;
    let mut checked = Vec::with_capacity(panels.len());
    for raw in panels {
        let panel: Panel = serde_json::from_value(raw.clone()).map_err(|_| invalid())?;
        if panel.title.trim().is_empty()
            || panel.title.len() > 160
            || panel.aggregates.is_empty()
            || panel.limit == 0
        {
            return Err(Error::validation(
                "Each panel needs a title, at least one measure and a positive limit.",
            ));
        }
        checked.push(panel);
    }
    Ok(Definition::Dashboard(checked))
}

async fn prepare_panel(tx: &mut Transaction, panel: &Panel) -> Result<Json> {
    let args = json!({"model":panel.model,"domain":panel.domain,"groupby":panel.groupby,
        "aggregates":panel.aggregates,"order":panel.order,"limit":panel.limit});
    let call = ToolCall::new(
        ToolCallId::new("saved-panel").map_err(|e| Error::invariant(e.to_string()))?,
        "aggregate_records".into(),
        args,
    )
    .map_err(|e| Error::invariant(e.to_string()))?;
    let plan = aggregate::prepare_saved(tx, &call).await?;
    let records = tx.env().records(plan.model())?;
    let mut labels = Map::new();
    for key in plan.columns() {
        let (field, operation) = key
            .split_once(':')
            .map_or((key, None), |(f, op)| (f, Some(op)));
        let metadata = records
            .spec()
            .field(field)
            .ok_or_else(|| Error::invariant("Report field has disappeared."))?;
        let title = operation.map_or_else(
            || metadata.string.clone(),
            |op| format!("{} ({op})", metadata.string),
        );
        labels.insert(key.to_owned(), json!(title));
    }
    Ok(
        json!({"title":panel.title,"model":plan.model(),"source":records.spec().description,
        "labels":labels,"visualization":panel.visualization,"limit":panel.limit,"method":plan.rpc_method(),"args":plan.rpc_arguments()}),
    )
}

async fn read_report(env: &Env, tx: &mut Transaction, id: i64) -> Result<()> {
    if env.principal().scope().is_some_and(|scope| {
        !scope.models().allows("reporting.report") || !scope.methods().allows("read")
    }) {
        return Err(Error::access("This connection cannot read the report."));
    }
    let report = env.records("reporting.report")?.browse([id])?;
    if report.values(tx, "name").await?.is_empty() {
        return Err(Error::access("The report is unavailable."));
    }
    Ok(())
}

fn json_value(value: Json) -> Value {
    Value::Json(Arc::new(value))
}

async fn stored(this: &RecordSet<Insight>, tx: &mut Transaction) -> Result<(String, Definition)> {
    let records = this.as_records().ensure_one()?;
    records.check_access(tx, Operation::Read).await?;
    let Cell::Str(name) = records.cell(tx, "name").await? else {
        return Err(Error::invariant("Saved report has no title."));
    };
    let Cell::Str(raw) = records.cell(tx, "definition").await? else {
        return Err(Error::invariant("Saved report has no definition."));
    };
    Ok((name.to_string(), parse(&raw)?))
}

#[methods(Insight, addon = "ai_base")]
impl Insight {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<Insight>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let company = names.iter().any(|key| key == "company_id");
        let mut defaults = sup.call(tx, this.as_records(), (names,)).await?;
        if company && let Some(id) = this.env().company_id()? {
            defaults.insert("company_id".into(), Value::Int(id));
        }
        Ok(defaults)
    }

    #[compute(depends=["definition"])]
    async fn _compute_sources(
        this: &RecordSet<Insight>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Insight>,
    ) -> Result<()> {
        for (index, row) in this.as_records().iter().enumerate() {
            let value = match row.cell(tx, "definition").await? {
                Cell::Null => String::new(),
                Cell::Str(raw) if raw.trim().is_empty() => String::new(),
                Cell::Str(raw) => match parse(&raw) {
                    Ok(Definition::Report(id)) => format!("Dynamic report #{id}"),
                    Ok(Definition::Dashboard(panels)) => panels
                        .iter()
                        .map(|panel| format!("{} · {}", panel.title, panel.model))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(_) if row.ids().is_empty() => "Complete the query definition.".to_owned(),
                    Err(error) => return Err(error),
                },
                _ => return Err(Error::invariant("Invalid saved report definition.")),
            };
            out.set(index, Self::SOURCES, value.as_str())?;
        }
        Ok(())
    }

    #[compute(depends=["shared_group_id"], depends_context=["uid"])]
    async fn _compute_group_member(
        this: &RecordSet<Insight>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Insight>,
    ) -> Result<()> {
        let groups = this.env().principal().resolution().groups();
        for (index, row) in this.as_records().iter().enumerate() {
            let member = match row.cell(tx, "shared_group_id").await? {
                Cell::Id(id) => groups.ids().iter().any(|gid| gid.get() == id),
                Cell::Null => false,
                _ => return Err(Error::invariant("Invalid saved report group.")),
            };
            out.set(index, Self::GROUP_MEMBER, member)?;
        }
        Ok(())
    }

    #[search_method]
    fn _search_group_member(
        req: &FieldSearchRequest<'_>,
        cx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        let groups: Vec<_> = cx
            .env()
            .principal()
            .resolution()
            .groups()
            .ids()
            .iter()
            .map(|g| g.get())
            .collect();
        let matches = Domain::field("shared_group_id")?.in_(groups);
        let included = |value| match (req.operator(), req.operand()) {
            (Operator::Eq, Operand::Bool(b)) => Ok(value == *b),
            (Operator::Ne, Operand::Bool(b)) => Ok(value != *b),
            (Operator::In, Operand::List(b)) => Ok(b.contains(&Operand::Bool(value))),
            (Operator::NotIn, Operand::List(b)) => Ok(!b.contains(&Operand::Bool(value))),
            (Operator::In, Operand::Set(b)) => Ok(b.contains(&Operand::Bool(value))),
            (Operator::NotIn, Operand::Set(b)) => Ok(!b.contains(&Operand::Bool(value))),
            _ => Err(Error::validation(
                "Group membership needs a boolean filter.",
            )),
        };
        Ok(match (included(true)?, included(false)?) {
            (true, false) => matches,
            (false, true) => !matches,
            (true, true) => Domain::TRUE,
            (false, false) => Domain::FALSE,
        })
    }

    #[constrains("definition", "shared_group_id", "name", "company_id")]
    async fn _check_definition(this: &RecordSet<Insight>, tx: &mut Transaction) -> Result<()> {
        for row in this.as_records().iter() {
            if let Cell::Id(id) = row.cell(tx, "shared_group_id").await? {
                if !tx.has_group(this.env(), "base.group_erp_manager").await? {
                    return Err(Error::access(
                        "Publishing a report to a group requires ERP management rights.",
                    ));
                }
                let group = this.env().records("res.groups")?.browse([id])?;
                if group.cell(tx, "share").await? == Cell::Bool(true) {
                    return Err(Error::validation(
                        "Publish reports to internal groups only.",
                    ));
                }
            }
            let Cell::Str(raw) = row.cell(tx, "definition").await? else {
                return Err(Error::validation("Enter a report definition."));
            };
            match parse(&raw)? {
                Definition::Report(id) => read_report(this.env(), tx, id).await?,
                Definition::Dashboard(panels) => {
                    for panel in &panels {
                        prepare_panel(tx, panel).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Save a private dashboard of live aggregations or an installed dynamic report reference.
    #[method(sealed)]
    #[rpc(effect=write, model, assistant=offer)]
    async fn save_analysis(
        this: &RecordSet<Insight>,
        tx: &mut Transaction,
        name: String,
        definition: String,
    ) -> Result<Value> {
        if name.trim().is_empty() || name.len() > 160 {
            return Err(Error::validation("Enter a report name of up to 160 bytes."));
        }
        parse(&definition)?;
        let company = this
            .env()
            .company_id()?
            .ok_or_else(|| Error::access("Select a company."))?;
        let id = this
            .as_records()
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.trim().into()))
                        .set_cell("definition", Cell::Str(definition.into()))
                        .set_cell("company_id", Cell::Id(company))
                        .set_cell("shared_group_id", Cell::Null),
                ],
            )
            .await?
            .ensure_one()?
            .int()?;
        Ok(json_value(
            json!({"type":"ir.actions.act_window","name":name.trim(),
            "res_model":"ai.insight","res_id":id,"target":"current","views":[[false,"form"]]}),
        ))
    }

    #[method(sealed)]
    #[rpc(effect=read)]
    async fn action_open(this: &RecordSet<Insight>, tx: &mut Transaction) -> Result<Value> {
        let (name, def) = stored(this, tx).await?;
        let (tag, params) = match def {
            Definition::Report(id) => {
                read_report(this.env(), tx, id).await?;
                ("reporting.report", json!({"report":id}))
            }
            Definition::Dashboard(_) => (TAG, json!({"insight":this.as_records().int()?})),
        };
        Ok(json_value(
            json!({"type":"ir.actions.client","name":name,"tag":tag,"target":"current","params":params}),
        ))
    }

    #[method(sealed)]
    #[rpc(effect=read)]
    async fn erp_analysis_queries(
        this: &RecordSet<Insight>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let (name, def) = stored(this, tx).await?;
        let Definition::Dashboard(panels) = def else {
            return Err(Error::validation(
                "Open the dynamic report in its report viewer.",
            ));
        };
        let mut queries = Vec::with_capacity(panels.len());
        for panel in &panels {
            queries.push(prepare_panel(tx, panel).await?);
        }
        Ok(json_value(json!({"name":name,"panels":queries})))
    }
}

fn actions() -> Result<Vec<ClientActionDecl>> {
    Ok(vec![ClientActionDecl::new(
        TAG,
        "Saved dashboard",
        |env, tx, params| {
            Box::pin(async move {
                let id = params
                    .get("insight")
                    .and_then(Json::as_i64)
                    .filter(|id| *id > 0 && params.len() == 1)
                    .ok_or_else(|| Error::validation("Select one saved report."))?;
                let selected = env.records("ai.insight")?.browse([id])?;
                if selected.values(tx, "name").await?.is_empty() {
                    return Err(Error::access("Saved report is unavailable."));
                }
                Ok(params.clone())
            })
        },
    )?])
}
erp_sdk::client_action_declarations!(actions);

#[cfg(test)]
mod tests;
