//! The VM never borrows an ORM transaction: it sends typed, capacity-one requests to its owner.
mod host;
pub(super) mod value;

use super::{ActionResult, ServerActionError};
use erp_sdk::prelude::*;
use erp_sdk::script::{
    Evaluation, HostError, LuauSandbox, ScriptCancellation, ScriptContext, ScriptError,
    ScriptLimits, ScriptValue,
};
use erp_sdk::{Env, Records};
use host::Call;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::{Semaphore, mpsc};
use value::Budget;

static PERMITS: OnceLock<std::result::Result<Arc<Semaphore>, String>> = OnceLock::new();
const MAX_CALLS: usize = 1024;

struct Request {
    call: Call,
    reply: std::sync::mpsc::SyncSender<Result<ScriptValue>>,
}
struct CancelOnDrop(ScriptCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Preserve a primary ORM error (including SQLSTATE/source), not a new error with its text.
pub(super) fn script_error(error: ScriptError) -> Error {
    match error {
        ScriptError::Host { function, source } => match source.downcast::<Error>() {
            Ok(error) => *error,
            Err(source) => Error::user(format!("Script host function {function} failed"))
                .with_source(ScriptError::Host { function, source }),
        },
        other => Error::user(other.to_string()).with_source(other),
    }
}
fn cancelled() -> Error {
    script_error(ScriptError::Cancelled)
}
fn timeout() -> Error {
    script_error(ScriptError::LimitExceeded {
        limit: erp_sdk::script::LimitKind::Time,
        max: 5000,
    })
}

pub(super) async fn evaluate(
    tx: &mut Transaction,
    env: &Env,
    records: &Records,
    source: String,
) -> Result<Evaluation> {
    check_active(records)?;
    if source.len() > ScriptLimits::MAX_SOURCE_BYTES {
        return Err(Error::validation(
            "Server action script source exceeds 262144 bytes",
        ));
    }
    let permits = PERMITS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| Arc::new(Semaphore::new(n.get())))
            .map_err(|e| e.to_string())
    });
    let permits = permits.as_ref().map_err(|message| {
        Error::programming(format!(
            "Cannot determine script worker capacity: {message}"
        ))
    })?;
    let permit = acquire_worker(permits)?;
    let cancellation = ScriptCancellation::default();
    let _cancel = CancelOnDrop(cancellation.clone());
    let deadline = Instant::now()
        .checked_add(ScriptLimits::MAX_TIME)
        .ok_or_else(|| Error::programming("Script deadline overflow"))?;
    let async_deadline = tokio::time::Instant::from_std(deadline);
    let (sender, mut receiver) = mpsc::channel::<Request>(1);
    let mut context = ScriptContext::new();
    let map = env.context().to_map_bounded(256)?;
    let mut budget = Budget::default();
    let mut entries = Vec::new();
    for (k, v) in map {
        budget.charge(k.len(), 0)?;
        entries.push((k, value::from_value(&v, &mut budget, 1)?));
    }
    let context_value = ScriptValue::Dict(entries.into_iter().collect());
    context
        .set_value("env.context", context_value.clone())
        .map_err(script_error)?;
    context
        .set_value("context", context_value)
        .map_err(script_error)?;
    context
        .set_value("env.uid", env.uid())
        .map_err(script_error)?;
    let descriptors: Vec<_> = records
        .ids()
        .iter()
        .map(|id| host::descriptor(records.model_name(), *id))
        .collect();
    context
        .set_value(
            "record",
            descriptors.first().cloned().unwrap_or(ScriptValue::Nil),
        )
        .map_err(script_error)?;
    context
        .set_value("records", ScriptValue::List(descriptors))
        .map_err(script_error)?;
    context.capture("action").map_err(script_error)?;
    for name in ["search", "read", "write", "create", "unlink", "ref", "log"] {
        let sender = sender.clone();
        let token = cancellation.clone();
        let full_name = if name == "log" {
            name.to_owned()
        } else {
            format!("env.{name}")
        };
        context
            .set_fn(&full_name, move |args| {
                if token.is_cancelled() {
                    return Err(HostError::new(cancelled()));
                }
                let call = Call::parse(name, args)?;
                let (reply, receive) = std::sync::mpsc::sync_channel(1);
                sender
                    .blocking_send(Request { call, reply })
                    .map_err(|_| HostError::new(cancelled()))?;
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| HostError::new(timeout()))?;
                receive
                    .recv_timeout(remaining)
                    .map_err(|e| match e {
                        std::sync::mpsc::RecvTimeoutError::Timeout => HostError::new(timeout()),
                        std::sync::mpsc::RecvTimeoutError::Disconnected => {
                            HostError::new(cancelled())
                        }
                    })?
                    .map_err(HostError::new)
            })
            .map_err(script_error)?;
    }
    drop(sender);
    let mut task = spawn_vm(permit, source, context, cancellation);
    let mut calls = 0usize;
    let mut logs = String::new();
    let mut closed = false;
    loop {
        tokio::select! {
            biased;
            joined=&mut task => {
                let output=joined.map_err(|e|Error::programming(format!("Server action VM task failed: {e}")))?.map_err(script_error)?;
                if !logs.is_empty() {tracing::info!(target:"erp::server_action", uid=env.uid(), message=%logs, "script log");}
                return Ok(output);
            },
            next=receiver.recv(), if !closed => {
                let Some(Request{call,reply})=next else { closed=true; continue; };
                calls=calls.checked_add(1).ok_or_else(||Error::validation("Server action host call count overflow"))?;
                let result=if calls>MAX_CALLS {Err(Error::validation("Server action exceeds 1024 host calls"))}
                    else {match tokio::time::timeout_at(async_deadline,call.serve(tx,env,&mut logs)).await {Ok(result)=>result,Err(_)=>Err(timeout())}};
                if let Err(undelivered)=reply.send(result) {return match undelivered.0 {Err(error)=>Err(error),Ok(_)=>Err(cancelled())};}
            },
            ()=tokio::time::sleep_until(async_deadline)=>return Err(timeout()),
        }
    }
}

pub(super) fn action_result(mut output: Evaluation) -> Result<Option<ActionResult>> {
    let captured = output
        .captured
        .shift_remove("action")
        .ok_or_else(|| Error::programming("Script action capture is absent"))?;
    if !captured.is_nil() && !output.value.is_nil() {
        return Err(ServerActionError::AmbiguousResult.into());
    }
    let selected = if captured.is_nil() {
        output.value
    } else {
        captured
    };
    if selected.is_nil() {
        return Ok(None);
    }
    let mut b = Budget::default();
    value::check(&selected, &mut b, 0)?;
    let json = erp_sdk::serde_json::Value::try_from(selected).map_err(script_error)?;
    Ok(Some(ActionResult::new(json)?))
}

pub(super) fn equation_value(value: ScriptValue) -> Result<Value> {
    let mut budget = Budget::default();
    value::check(&value, &mut budget, 0)?;
    value::into_value(value)
}

pub(super) fn checked_values(
    records: &Records,
    values: &erp_sdk::ValueMap,
) -> Result<erp_sdk::RawVals> {
    host::safe_vals(records, values)
}

fn acquire_worker(pool: &Arc<Semaphore>) -> Result<tokio::sync::OwnedSemaphorePermit> {
    Arc::clone(pool)
        .try_acquire_owned()
        .map_err(|_| ServerActionError::ScriptBusy.into())
}

fn spawn_vm(
    permit: tokio::sync::OwnedSemaphorePermit,
    source: String,
    context: ScriptContext,
    cancellation: ScriptCancellation,
) -> tokio::task::JoinHandle<std::result::Result<Evaluation, ScriptError>> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        LuauSandbox::default().run_cancellable(source.as_bytes(), &context, &cancellation)
    })
}

pub(super) fn check_active(records: &Records) -> Result<()> {
    check_active_count(records.len())
}

pub(super) fn check_active_count(count: usize) -> Result<()> {
    if count > host::MAX_RECORDS {
        return Err(Error::validation(
            "Server action script accepts at most 256 active records",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    #[tokio::test]
    async fn vm_holds_permit_until_cancelled_worker_exits() {
        let pool = Arc::new(Semaphore::new(1));
        let permit = acquire_worker(&pool).unwrap();
        let token = ScriptCancellation::default();
        let (started, wait) = tokio::sync::oneshot::channel();
        let started = std::sync::Mutex::new(Some(started));
        let (release, blocking) = std::sync::mpsc::sync_channel(1);
        let blocking = std::sync::Mutex::new(blocking);
        let mut context = ScriptContext::new();
        context
            .set_fn("hold", move |_| {
                if let Some(sender) = started
                    .lock()
                    .map_err(|_| HostError::new(Error::programming("test start mutex poisoned")))?
                    .take()
                {
                    sender
                        .send(())
                        .map_err(|_| HostError::new(Error::programming("test observer gone")))?;
                }
                blocking
                    .lock()
                    .map_err(|_| HostError::new(Error::programming("test release mutex poisoned")))?
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .map_err(|e| HostError::new(Error::programming(e.to_string())))?;
                Ok(ScriptValue::Nil)
            })
            .unwrap();
        let worker = spawn_vm(
            permit,
            "hold(); while true do end".to_owned(),
            context,
            token.clone(),
        );
        wait.await.unwrap();
        token.cancel();
        let busy = acquire_worker(&pool).unwrap_err();
        assert_eq!(busy.kind(), erp_sdk::ErrorKind::ResourceExhausted);
        assert_eq!(
            busy.message(),
            "All server action script workers are busy; the run was refused."
        );
        assert!(matches!(
            std::error::Error::source(&busy).and_then(|e| e.downcast_ref::<ServerActionError>()),
            Some(ServerActionError::ScriptBusy)
        ));
        release.send(()).unwrap();
        assert!(matches!(worker.await.unwrap(), Err(ScriptError::Cancelled)));
        assert!(pool.try_acquire_owned().is_ok());
    }
}
