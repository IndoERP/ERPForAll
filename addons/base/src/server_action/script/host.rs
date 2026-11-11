//! Typed requests, validated on the VM thread before they enter the capacity-one channel.
use super::value::{self, Budget};
use erp_sdk::prelude::*;
use erp_sdk::script::{ScriptValue, XmlId, env_ref};
use erp_sdk::{Domain, Env, Load, Operation, SearchOptions, ValueMap};

pub(super) const MAX_RECORDS: usize = 256;
const MAX_FIELDS: usize = 32;
pub(super) const MAX_LOG_BYTES: usize = 65_536;

pub(super) enum Call {
    Search {
        model: String,
        domain: Domain,
        limit: u64,
    },
    Read {
        model: String,
        ids: Vec<i64>,
        fields: Vec<String>,
    },
    Write {
        model: String,
        ids: Vec<i64>,
        values: ValueMap,
    },
    Create {
        model: String,
        values: ValueMap,
    },
    Unlink {
        model: String,
        ids: Vec<i64>,
    },
    Ref(XmlId),
    Log(String),
}
fn invalid(reason: &str) -> Error {
    Error::validation(format!("Server action host call: {reason}"))
}
fn text(v: &ScriptValue) -> Result<&str> {
    match v {
        ScriptValue::String(x) => Ok(x),
        _ => Err(invalid("expected a string")),
    }
}
fn list(v: &ScriptValue) -> Result<&[ScriptValue]> {
    match v {
        ScriptValue::List(x) => Ok(x),
        ScriptValue::Dict(x) if x.is_empty() => Ok(&[]),
        _ => Err(invalid("expected an array")),
    }
}
fn ids(v: &ScriptValue) -> Result<Vec<i64>> {
    let xs = list(v)?;
    if xs.len() > MAX_RECORDS {
        return Err(invalid("at most 256 record IDs are allowed"));
    }
    let mut seen = std::collections::BTreeSet::new();
    xs.iter()
        .map(|x| match x {
            ScriptValue::Int(i) if *i > 0 && seen.insert(*i) => Ok(*i),
            _ => Err(invalid("IDs must be positive unique integers")),
        })
        .collect()
}
fn values(v: &ScriptValue) -> Result<ValueMap> {
    let ScriptValue::Dict(map) = v else {
        return Err(invalid("expected a field dictionary"));
    };
    if map.len() > MAX_FIELDS || map.contains_key("id") {
        return Err(invalid("at most 32 fields; id is not writable"));
    }
    map.iter()
        .map(|(k, v)| Ok((k.clone(), value::into_value(v.clone())?)))
        .collect()
}
impl Call {
    pub(super) fn parse(name: &str, args: &[ScriptValue]) -> Result<Self> {
        let mut budget = Budget::default();
        for arg in args {
            value::check(arg, &mut budget, 0)?;
        }
        match (name, args) {
            ("search", [m, d]) => {
                Self::parse("search", &[m.clone(), d.clone(), ScriptValue::Int(256)])
            }
            ("search", [m, d, ScriptValue::Int(limit)]) if (1..=256).contains(limit) => {
                let domain_json =
                    erp_sdk::serde_json::Value::try_from(d.clone()).map_err(super::script_error)?;
                // Empty Luau tables are dictionaries; the domain's empty-list spelling is equivalent.
                let domain_json = if matches!(d, ScriptValue::Dict(x) if x.is_empty()) {
                    erp_sdk::serde_json::json!([])
                } else {
                    domain_json
                };
                Ok(Self::Search {
                    model: text(m)?.to_owned(),
                    domain: Domain::from_json(&domain_json)?,
                    limit: u64::try_from(*limit).map_err(|_| invalid("invalid search limit"))?,
                })
            }
            ("read", [m, i, f]) => {
                let fields = list(f)?;
                if fields.is_empty() || fields.len() > MAX_FIELDS {
                    return Err(invalid("read requires 1 to 32 explicit fields"));
                }
                Ok(Self::Read {
                    model: text(m)?.to_owned(),
                    ids: ids(i)?,
                    fields: fields
                        .iter()
                        .map(|f| Ok(text(f)?.to_owned()))
                        .collect::<Result<_>>()?,
                })
            }
            ("write", [m, i, v]) => Ok(Self::Write {
                model: text(m)?.to_owned(),
                ids: ids(i)?,
                values: values(v)?,
            }),
            ("create", [m, v]) => Ok(Self::Create {
                model: text(m)?.to_owned(),
                values: values(v)?,
            }),
            ("unlink", [m, i]) => Ok(Self::Unlink {
                model: text(m)?.to_owned(),
                ids: ids(i)?,
            }),
            ("ref", [x]) => {
                let xid = XmlId::parse(text(x)?).map_err(|e| invalid(&e.to_string()))?;
                if xid.module().is_none() {
                    return Err(invalid("ref requires module.name"));
                }
                Ok(Self::Ref(xid))
            }
            ("log", [x]) => {
                let value = text(x)?;
                if value.len() > MAX_LOG_BYTES {
                    return Err(invalid("log exceeds 65536 bytes"));
                }
                Ok(Self::Log(value.to_owned()))
            }
            _ => Err(invalid(
                "wrong arguments (search limit must be between 1 and 256)",
            )),
        }
    }
    pub(super) async fn serve(
        self,
        tx: &mut Transaction,
        env: &Env,
        log: &mut String,
    ) -> Result<ScriptValue> {
        match self {
            Self::Search {
                model,
                domain,
                limit,
            } => {
                let found = env
                    .records(&model)?
                    .search_with(tx, &domain, &SearchOptions::new().limit(limit))
                    .await?;
                Ok(ScriptValue::List(
                    found.ids().iter().map(|i| descriptor(&model, *i)).collect(),
                ))
            }
            Self::Read { model, ids, fields } => {
                let records = env.records(&model)?.browse(ids)?;
                records.check_access(tx, Operation::Read).await?;
                let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
                let rows = records.read_with(tx, &fields, Load::Raw).await?;
                let mut b = Budget::default();
                let mut out = Vec::new();
                for row in rows {
                    let mut entries = Vec::new();
                    for (k, v) in row {
                        b.charge(k.len(), 0)?;
                        entries.push((k, value::from_value(&v, &mut b, 1)?));
                    }
                    out.push(ScriptValue::Dict(entries.into_iter().collect()));
                }
                Ok(ScriptValue::List(out))
            }
            Self::Write { model, ids, values } => {
                let r = env.records(&model)?.browse(ids)?;
                let v = safe_vals(&r, &values)?;
                r.write(tx, v).await?;
                Ok(ScriptValue::Nil)
            }
            Self::Create { model, values } => {
                let r = env.records(&model)?;
                let v = safe_vals(&r, &values)?;
                let found = r.create(tx, vec![v]).await?;
                let id = *found
                    .ids()
                    .first()
                    .ok_or_else(|| Error::programming("script create returned no record"))?;
                Ok(descriptor(&model, id))
            }
            Self::Unlink { model, ids } => {
                env.records(&model)?.browse(ids)?.unlink(tx).await?;
                Ok(ScriptValue::Nil)
            }
            Self::Ref(xid) => {
                let r = env_ref(tx, env, &xid, true)
                    .await?
                    .ok_or_else(|| Error::missing("External ID has no record"))?;
                r.check_access(tx, Operation::Read).await?;
                let id = *r
                    .ids()
                    .first()
                    .ok_or_else(|| Error::missing("External ID has no record"))?;
                Ok(descriptor(r.model_name(), id))
            }
            Self::Log(text) => {
                if log
                    .len()
                    .checked_add(text.len())
                    .filter(|n| *n <= MAX_LOG_BYTES)
                    .is_none()
                {
                    return Err(invalid("log exceeds 65536 bytes"));
                }
                log.push_str(&text);
                Ok(ScriptValue::Nil)
            }
        }
    }
}
pub(super) fn descriptor(model: &str, id: i64) -> ScriptValue {
    ScriptValue::Dict(
        [
            ("id".to_owned(), ScriptValue::Int(id)),
            ("model".to_owned(), ScriptValue::String(model.to_owned())),
        ]
        .into_iter()
        .collect(),
    )
}

pub(super) fn safe_vals(records: &erp_sdk::Records, values: &ValueMap) -> Result<erp_sdk::RawVals> {
    let raw = records.vals_from_values(values.iter())?;
    let mut work = 0usize;
    check_commands(records, &raw, 0, records.len().max(1), &mut work)?;
    Ok(raw)
}

/// Every nested command is checked before any mutation; nested writes cannot bypass HTML refusal.
fn check_commands(
    records: &erp_sdk::Records,
    raw: &erp_sdk::RawVals,
    depth: usize,
    multiplicity: usize,
    work: &mut usize,
) -> Result<()> {
    if depth > 32 || raw.len() > MAX_FIELDS {
        return Err(invalid("nested values exceed field/depth limits"));
    }
    for (name, _) in raw.iter() {
        if records
            .spec()
            .field(name)
            .is_some_and(|f| matches!(f.ty, erp_sdk::FieldType::Html))
        {
            return Err(super::super::ServerActionError::Unsupported {
                feature: format!(
                    "Script update of the html field {}.{name}",
                    records.model_name()
                ),
                task: "a later release",
            }
            .into());
        }
    }
    for (name, commands) in raw.iter_commands() {
        let field = records
            .spec()
            .field(name)
            .ok_or_else(|| invalid("unknown command field"))?;
        let model = field
            .ty
            .comodel()
            .ok_or_else(|| invalid("commands require a relational field"))?;
        let target = records.env().records(model)?;
        for command in commands {
            let count = match command {
                Command::Set(ids) => ids.len(),
                Command::Create(_)
                | Command::Update(_, _)
                | Command::Delete(_)
                | Command::Unlink(_)
                | Command::Link(_)
                | Command::Clear => 1,
            };
            *work = work
                .checked_add(
                    count
                        .checked_mul(multiplicity)
                        .ok_or_else(|| invalid("command work overflow"))?,
                )
                .ok_or_else(|| invalid("command work overflow"))?;
            if *work > MAX_RECORDS {
                return Err(invalid("at most 256 command records are allowed"));
            }
            match command {
                Command::Create(values) | Command::Update(_, values) => {
                    check_commands(&target, values, depth + 1, multiplicity, work)?
                }
                Command::Delete(_)
                | Command::Unlink(_)
                | Command::Link(_)
                | Command::Clear
                | Command::Set(_) => {}
            }
        }
    }
    Ok(())
}
