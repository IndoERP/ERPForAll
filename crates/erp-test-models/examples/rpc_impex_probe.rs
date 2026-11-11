//! Private, in-process integer impex lower-bound probe, not a served-floor benchmark.
//! Usage: rpc_impex_probe <binder|orm|floor> <load|export|templates> <100|1000|10000> <result-path>
//! ORM means the current SAME-BINARY implementation, never a measured main-build baseline.
//! Linux VmHWM includes fixture construction and warm-up; timings exclude both.
#![allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    reason = "measurement CLI"
)]

use erp_fields::{Value, ValueMap};
use erp_orm::{
    Transaction,
    rpc::{Exposed, Exposure, RpcArgs},
};
use erp_test_models::{base_users, test_impex};
use erp_testkit::Harness;
use std::{error::Error, io, path::PathBuf, sync::Arc, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn invalid(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Binder,
    Orm,
    Floor,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Load,
    Export,
    Templates,
}

struct Input {
    mode: Mode,
    operation: Operation,
    size: usize,
    output: PathBuf,
    mode_name: String,
    operation_name: String,
}

impl Input {
    fn parse() -> Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        let [_, mode, operation, size, output] = args.as_slice() else {
            return Err(invalid(
                "usage: rpc_impex_probe <binder|orm|floor> <load|export|templates> <100|1000|10000> <result-path>",
            ));
        };
        if mode == "orm" && operation == "templates" {
            return Err(invalid(
                "orm templates is unavailable: no direct ORM get_import_templates API exists",
            ));
        }
        Ok(Self {
            mode: match mode.as_str() {
                "binder" => Mode::Binder,
                "orm" => Mode::Orm,
                "floor" => Mode::Floor,
                _ => return Err(invalid("mode must be binder, orm or floor")),
            },
            operation: match operation.as_str() {
                "load" => Operation::Load,
                "export" => Operation::Export,
                "templates" => Operation::Templates,
                _ => return Err(invalid("operation must be load, export or templates")),
            },
            size: match size.as_str() {
                "100" => 100,
                "1000" => 1000,
                "10000" => 10000,
                _ => return Err(invalid("size must be 100, 1000 or 10000")),
            },
            output: PathBuf::from(output),
            mode_name: mode.clone(),
            operation_name: operation.clone(),
        })
    }
}

fn args(operation: Operation, data: &[Vec<String>]) -> RpcArgs {
    let mut map = ValueMap::new();
    match operation {
        Operation::Load => {
            map.insert("fields".into(), Value::list([Value::str("value")]));
            map.insert(
                "data".into(),
                Value::list(
                    data.iter()
                        .map(|row| Value::list(row.iter().map(Value::str))),
                ),
            );
        }
        Operation::Export => {
            map.insert(
                "fields_to_export".into(),
                Value::list([Value::str("value")]),
            );
        }
        Operation::Templates => {}
    }
    RpcArgs::new(map)
}

fn map_value(key: &str, value: Value) -> Value {
    let mut map = ValueMap::new();
    map.insert(key.into(), value);
    Value::Map(Arc::new(map))
}

async fn invoke(
    input: &Input,
    tx: &mut Transaction,
    exposed: Option<&Exposed>,
    ids: &[i64],
    data: &[Vec<String>],
    arguments: RpcArgs,
) -> Result<Value> {
    let records = tx.env().records("export.integer")?;
    let records = if input.operation == Operation::Export {
        records.browse(ids.iter().copied())?
    } else {
        records
    };
    if input.mode == Mode::Binder {
        let exposed =
            exposed.ok_or_else(|| invalid("binder mode requires an exposed RPC method"))?;
        return Ok(exposed.call(tx, records, arguments).await?);
    }
    match (input.mode, input.operation) {
        (Mode::Orm, Operation::Load) => Ok(erp_data::load(&records, tx, &["value"], data)
            .await?
            .to_value()),
        (Mode::Orm, Operation::Export) => {
            Ok(
                erp_data::export_data(&records, tx, &["value"], &erp_data::SuperuserOnly)
                    .await?
                    .to_value(),
            )
        }
        (Mode::Floor, Operation::Templates) => Ok(Value::list([])),
        (Mode::Orm, Operation::Templates) => {
            Err(invalid("no direct ORM get_import_templates API exists"))
        }
        (Mode::Floor, Operation::Load) => {
            let values = data
                .iter()
                .map(|row| -> Result<i32> {
                    let [cell] = row.as_slice() else {
                        return Err(invalid("integer probe requires one cell per row"));
                    };
                    Ok(cell.parse::<i32>()?)
                })
                .collect::<Result<Vec<_>>>()?;
            // test_impex's const default is 4. write.rs applies uid and NOW_UTC for log_access;
            // ddl.rs gives id an identity default and leaves other defaults to the ORM.
            let uid = tx.env().uid();
            let sql = format!(
                "INSERT INTO export_integer (value, const, create_uid, write_uid, create_date, write_date) SELECT v, 4, $2, $2, {now}, {now} FROM unnest($1::int4[]) WITH ORDINALITY AS x(v, ord) ORDER BY ord RETURNING id",
                now = erp_sql::NOW_UTC
            );
            let rows = tx.query(&sql, &[&values, &uid]).await?;
            let mut map = ValueMap::new();
            map.insert(
                "ids".into(),
                Value::list(rows.iter().map(|row| Value::Int(row.get(0)))),
            );
            map.insert("messages".into(), Value::list([]));
            map.insert("nextrow".into(), Value::Int(0));
            Ok(Value::Map(Arc::new(map)))
        }
        (Mode::Floor, Operation::Export) => {
            let rows = tx.query("SELECT t.value FROM unnest($1::int8[]) WITH ORDINALITY AS x(id, ord) JOIN export_integer t ON t.id = x.id ORDER BY x.ord", &[&ids]).await?;
            Ok(map_value(
                "datas",
                Value::list(
                    rows.iter()
                        .map(|row| Value::list([Value::Int(i64::from(row.get::<_, i32>(0)))])),
                ),
            ))
        }
        (Mode::Binder, _) => Err(invalid("binder branch must have returned")),
    }
}

async fn prepare(h: &Harness, input: &Input) -> Result<(Transaction, Vec<i64>)> {
    let mut tx = h.begin().await?;
    tx.open().await?;
    tx.batch("TRUNCATE export_integer RESTART IDENTITY CASCADE")
        .await?;
    let ids = if input.operation == Operation::Export {
        let count = i64::try_from(input.size)?;
        tx.query("INSERT INTO export_integer (value, const) SELECT v::int4, 4 FROM generate_series(1, $1::int8) AS x(v) ORDER BY v RETURNING id", &[&count]).await?
            .iter().map(|row| row.get(0)).collect()
    } else {
        Vec::new()
    };
    tx.invalidate_all().await?;
    Ok((tx, ids))
}

fn peak_bytes() -> Result<u64> {
    if !cfg!(target_os = "linux") {
        return Err(invalid("this probe requires Linux VmHWM"));
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    // /proc/self/status contains unrelated kernel fields; select only the exact VmHWM key.
    let mut lines = status
        .lines()
        .filter_map(|line| line.strip_prefix("VmHWM:"));
    let line = lines.next().ok_or_else(|| invalid("VmHWM missing"))?;
    if lines.next().is_some() {
        return Err(invalid("duplicate VmHWM"));
    }
    let parts: Vec<_> = line.split_whitespace().collect();
    let [amount, "kB"] = parts.as_slice() else {
        return Err(invalid("malformed VmHWM"));
    };
    let bytes = amount
        .parse::<u64>()?
        .checked_mul(1024)
        .ok_or_else(|| invalid("VmHWM overflow"))?;
    if bytes == 0 {
        return Err(invalid("VmHWM is zero"));
    }
    Ok(bytes)
}

async fn run(input: &Input, h: &Harness) -> Result<()> {
    let method = match input.operation {
        Operation::Load => "load",
        Operation::Export => "export_data",
        Operation::Templates => "get_import_templates",
    };
    let model = h
        .registry()
        .model_id("export.integer")
        .ok_or_else(|| invalid("fixture model missing"))?;
    let exposed = if input.mode == Mode::Binder {
        match h.orm().exposure().get(model, method) {
            Some(Exposure::Exposed(exposed)) => Some(exposed),
            other => {
                return Err(invalid(format!(
                    "binder mode requires fixture RPC exposure: {other:?}"
                )));
            }
        }
    } else {
        None
    };
    // Refuse schema drift rather than silently measuring an incomplete SQL floor.
    let mut schema = h.begin().await?;
    let columns = schema.query("SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'export_integer' ORDER BY column_name", &[]).await?
        .iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>();
    let expected = [
        "const",
        "create_date",
        "create_uid",
        "id",
        "value",
        "write_date",
        "write_uid",
    ];
    if columns.iter().map(String::as_str).collect::<Vec<_>>() != expected {
        return Err(invalid(format!(
            "unexpected export_integer columns: {columns:?}"
        )));
    }
    schema.batch("INSERT INTO res_users (id, name, login, active) VALUES (1, 'probe', 'probe', true) ON CONFLICT (id) DO NOTHING").await?;
    schema.commit().await?;
    let data = (1..=input.size)
        .map(|value| vec![value.to_string()])
        .collect::<Vec<_>>();
    let (mut warm, ids) = prepare(h, input).await?;
    let warmed = invoke(
        input,
        &mut warm,
        exposed,
        &ids,
        &data,
        args(input.operation, &data),
    )
    .await?;
    std::hint::black_box(warmed);
    warm.rollback().await?;
    let (mut tx, ids) = prepare(h, input).await?;
    let calls = if input.operation == Operation::Templates {
        input.size
    } else {
        1
    };
    let arguments = (0..calls)
        .map(|_| args(input.operation, &data))
        .collect::<Vec<_>>();
    let before = tx.query_count();
    let start = Instant::now();
    let mut output = Value::False;
    for arguments in arguments {
        drop(output);
        output = invoke(input, &mut tx, exposed, &ids, &data, arguments).await?;
        std::hint::black_box(&output);
    }
    let elapsed_ns = start.elapsed().as_nanos();
    let statements = tx.query_count() - before;
    let peak = peak_bytes()?;
    let json = erp_fields::convert::value_to_json(&output)?;
    match input.operation {
        Operation::Load => {
            let returned = json["ids"]
                .as_array()
                .ok_or_else(|| invalid("load ids missing"))?;
            let stored = tx
                .query(
                    "SELECT id, value, const FROM export_integer ORDER BY id",
                    &[],
                )
                .await?;
            if returned.len() != input.size
                || stored.len() != input.size
                || json["messages"] != serde_json::json!([])
                || json["nextrow"] != 0
            {
                return Err(invalid("load result shape mismatch"));
            }
            for (index, row) in stored.iter().enumerate() {
                if returned[index] != row.get::<_, i64>(0)
                    || i64::from(row.get::<_, i32>(1)) != i64::try_from(index + 1)?
                    || row.get::<_, i32>(2) != 4
                {
                    return Err(invalid("load result differs from database/input"));
                }
            }
        }
        Operation::Export => {
            let expected = (1..=input.size)
                .map(|value| vec![value])
                .collect::<Vec<_>>();
            if json != serde_json::json!({"datas": expected}) {
                return Err(invalid("export result differs from seed"));
            }
        }
        Operation::Templates => {
            if json != serde_json::json!([]) {
                return Err(invalid("templates result is not empty"));
            }
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&input.output)?;
    serde_json::to_writer(&mut file, &json)?;
    tx.rollback().await?;
    println!(
        "{}",
        serde_json::json!({"mode": input.mode_name, "operation": input.operation_name, "size": input.size, "calls": calls, "elapsed_ns": elapsed_ns.to_string(), "statements": statements, "peak_at_measurement_end_bytes": peak, "peak_source": "Linux VmHWM sampled after operation, including fixture and warmup; before result verification/serialization", "orm_baseline": "SAME-BINARY ORM; not main", "scope": "direct in-process binder/ORM/private SQL; not served floor", "result_path": input.output})
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let input = Input::parse()?;
    peak_bytes()?;
    let h = Harness::builder([
        &base_users::ADDON,
        &test_impex::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await?;
    let outcome = run(&input, &h).await;
    let cleanup = h.cleanup().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(cleanup)) => Err(invalid(format!(
            "probe failed: {error}; database cleanup also failed: {cleanup}"
        ))),
    }
}
