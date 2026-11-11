//! The host side of `orm-host`: identity from the host, capability gating, limits.

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use std::sync::{Arc, Mutex};

use erp_wasm_host::{
    BackendFuture, BoundaryError, BoundaryErrorKind, BridgeResult, Caller, ColumnData,
    ColumnVector, HostError, Limits, LogLevel, OrmBackend, OrmBridge, OrmCapabilities, OrmOp,
    RecordBatch, ScalarValue, WasmHost, X2manyCommand,
};

/// Records what the backend saw: `(operation, user id, company id)`.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, i64, i64)>>,
    logs: Mutex<Vec<String>>,
    created: Mutex<Option<usize>>,
    /// Column count the read answer carries (default: one per requested field).
    read_columns: Mutex<Option<usize>>,
    /// Answer of `call_method` (default `null`).
    call_answer: Mutex<Option<String>>,
    /// When set, `call_method` re-enters the bridge, as a backend dispatching into another addon.
    nest: Mutex<Option<OrmBridge>>,
    /// How many times `call_method` ran, and the error that stopped the nesting.
    nested_runs: Mutex<u32>,
    nested_stop: Mutex<Option<BoundaryError>>,
}

impl Recorder {
    fn note(&self, op: &str, caller: &Caller) {
        self.calls.lock().expect("lock").push((
            op.to_owned(),
            caller.user_id(),
            caller.company_id(),
        ));
    }
    fn calls(&self) -> Vec<(String, i64, i64)> {
        self.calls.lock().expect("lock").clone()
    }
}

impl OrmBackend for Recorder {
    fn search<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        _domain_json: &'a str,
        _limit: Option<u32>,
        _offset: Option<u32>,
        _order: Option<&'a str>,
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
        self.note("search", caller);
        Box::pin(async { Ok(vec![1, 2, 3]) })
    }
    fn read_columns<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        fields: &'a [String],
    ) -> BackendFuture<'a, BridgeResult<RecordBatch>> {
        self.note("read", caller);
        let count = self
            .read_columns
            .lock()
            .expect("lock")
            .unwrap_or(fields.len());
        Box::pin(async move {
            Ok(RecordBatch {
                model: model.to_owned(),
                ids: ids.to_vec(),
                columns: (0..count)
                    .map(|i| ColumnVector {
                        field: format!("f{i}"),
                        data: ColumnData::ColInt(vec![0; ids.len()]),
                    })
                    .collect(),
            })
        })
    }
    fn create_batch<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        values: &'a [Vec<(String, ScalarValue)>],
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
        self.note("create", caller);
        let n = self.created.lock().expect("lock").unwrap_or(values.len());
        Box::pin(async move { Ok((1..=n as i64).collect()) })
    }
    fn write_batch<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        _ids: &'a [i64],
        _values: &'a [(String, ScalarValue)],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        self.note("write", caller);
        Box::pin(async { Ok(()) })
    }
    fn write_columns<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        _ids: &'a [i64],
        _columns: &'a [ColumnVector],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        self.note("write-columns", caller);
        Box::pin(async { Ok(()) })
    }
    fn unlink_batch<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        _ids: &'a [i64],
    ) -> BackendFuture<'a, BridgeResult<()>> {
        self.note("unlink", caller);
        Box::pin(async { Ok(()) })
    }
    fn call_method<'a>(
        &'a self,
        caller: &'a Caller,
        _model: &'a str,
        _method: &'a str,
        _ids: &'a [i64],
        _args_json: &'a str,
    ) -> BackendFuture<'a, BridgeResult<String>> {
        self.note("call", caller);
        let answer = self.call_answer.lock().expect("lock").clone();
        let nested = self.nest.lock().expect("lock").clone();
        Box::pin(async move {
            if let Some(bridge) = nested {
                *self.nested_runs.lock().expect("lock") += 1;
                if let Err(e) = bridge.call_method("m", "ping", &[], "[]").await {
                    *self.nested_stop.lock().expect("lock") = Some(e);
                }
            }
            Ok(answer.unwrap_or_else(|| "null".to_owned()))
        })
    }
    fn log(&self, _caller: &Caller, _level: LogLevel, _target: &str, message: &str) {
        self.logs.lock().expect("lock").push(message.to_owned());
    }
}

fn caller(user: i64, company: i64) -> Caller {
    Caller::new(user, company, vec![company]).expect("caller")
}

fn bridge(caps: OrmCapabilities, limits: Limits) -> (OrmBridge, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    (
        OrmBridge::new(rec.clone(), caller(7, 3), caps, limits).with_any_method(),
        rec,
    )
}

fn denied(err: &BoundaryError, capability: &str) {
    assert_eq!(err.kind, BoundaryErrorKind::AccessDenied, "{err:?}");
    assert!(err.message.contains(capability), "{}", err.message);
}

/// guest-visible import path is covered when the dispatcher invokes a real guest)
#[tokio::test]
async fn backend_calls_carry_the_bound_identity_and_replacing_it_is_host_only() {
    let (mut b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    b.search("sale.order", "[]", None, None, None)
        .await
        .expect("search");
    b.set_caller(caller(9, 4));
    b.unlink_batch("sale.order", &[1]).await.expect("unlink");
    assert_eq!(
        rec.calls(),
        vec![("search".into(), 7, 3), ("unlink".into(), 9, 4)]
    );
}

/// interfaces)
#[tokio::test]
async fn every_operation_without_its_capability_is_access_denied_and_never_reaches_the_orm() {
    let (b, rec) = bridge(OrmCapabilities::NONE, Limits::default());
    let row = vec![vec![("name".to_owned(), ScalarValue::ValStr("x".into()))]];
    denied(
        &b.search("m", "[]", None, None, None)
            .await
            .expect_err("search"),
        "orm:read",
    );
    denied(
        &b.read_columns("m", &[1], &["a".into()])
            .await
            .expect_err("read"),
        "orm:read",
    );
    denied(
        &b.create_batch("m", &row).await.expect_err("create"),
        "orm:create",
    );
    denied(
        &b.write_batch("m", &[1], &row[0]).await.expect_err("write"),
        "orm:write",
    );
    denied(
        &b.write_columns("m", &[1], &[]).await.expect_err("cols"),
        "orm:write",
    );
    denied(
        &b.unlink_batch("m", &[1]).await.expect_err("unlink"),
        "orm:unlink",
    );
    denied(
        &b.call_method("m", "f", &[1], "[]").await.expect_err("call"),
        "orm:call-method",
    );
    denied(
        &b.super_call("m", "f", &[1], "[]").await.expect_err("super"),
        "orm:call-method",
    );
    assert!(
        rec.calls().is_empty(),
        "denied calls must not reach the ORM"
    );
}

#[tokio::test]
async fn a_read_only_addon_can_read_but_not_write() {
    let (b, rec) = bridge(OrmCapabilities::read_only(), Limits::default());
    assert_eq!(
        b.search("m", "[]", None, None, None).await.expect("search"),
        vec![1, 2, 3]
    );
    denied(
        &b.write_batch("m", &[1], &[]).await.expect_err("write"),
        "orm:write",
    );
    assert!(OrmCapabilities::read_only().allows(OrmOp::Read));
    assert!(!OrmCapabilities::read_only().allows(OrmOp::Write));
    assert_eq!(rec.calls().len(), 1);
}

#[tokio::test]
async fn batches_and_payloads_above_the_limits_are_resource_exhausted() {
    let limits = Limits::default()
        .with_batch_ids(2)
        .expect("ids")
        .with_payload_bytes(8)
        .expect("payload");
    let (b, rec) = bridge(OrmCapabilities::all(), limits);
    let err = b.unlink_batch("m", &[1, 2, 3]).await.expect_err("ids");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(err.message.contains("batch limit of 2"), "{}", err.message);
    let err = b
        .call_method("m", "f", &[1], "[0,0,0,0,0,0,0,0,0]")
        .await
        .expect_err("payload");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(err.message.contains("payload limit"), "{}", err.message);
    assert!(rec.calls().is_empty());
    // The backend's own answer is bounded too: three ids exceed a batch limit of two.
    let err = b
        .search("m", "[]", None, None, None)
        .await
        .expect_err("result");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
}

#[tokio::test]
async fn write_columns_with_a_short_column_is_invalid_argument() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    let columns = [ColumnVector {
        field: "qty".into(),
        data: ColumnData::ColInt(vec![1]),
    }];
    let err = b
        .write_columns("m", &[1, 2], &columns)
        .await
        .expect_err("short");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert!(
        err.message.contains("`qty` holds 1 values for 2 ids"),
        "{}",
        err.message
    );
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn empty_model_and_empty_field_list_are_invalid_argument() {
    let (b, _) = bridge(OrmCapabilities::all(), Limits::default());
    let err = b
        .search("", "[]", None, None, None)
        .await
        .expect_err("model");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    let err = b.read_columns("m", &[1], &[]).await.expect_err("fields");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
}

#[tokio::test]
async fn a_backend_that_creates_the_wrong_count_is_an_internal_error() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    *rec.created.lock().expect("lock") = Some(1);
    let rows = vec![vec![], vec![]];
    let err = b.create_batch("m", &rows).await.expect_err("count");
    assert_eq!(err.kind, BoundaryErrorKind::InternalError);
    assert!(
        err.message.contains("1 records for 2 rows"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn super_call_to_a_backend_without_a_dispatcher_is_refused_loudly() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    let err = b
        .super_call("m", "f", &[1], "[]")
        .await
        .expect_err("unsupported");
    assert_eq!(err.kind, BoundaryErrorKind::Unsupported);
    assert!(
        err.message.contains("override dispatcher"),
        "{}",
        err.message
    );
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn super_call_arguments_cannot_carry_identity() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    let err = b
        .super_call("m", "f", &[1], r#"{"uid": 1}"#)
        .await
        .expect_err("identity key");
    assert_eq!(err.kind, BoundaryErrorKind::AccessDenied);
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn super_call_needs_the_capabilities_of_the_running_method() {
    let caps = OrmCapabilities::NONE.with(OrmOp::CallMethod);
    let (b, rec) = bridge(caps, Limits::default());
    denied(
        &b.super_call("m", "unlink", &[1], "[]")
            .await
            .expect_err("unlink"),
        "orm:unlink",
    );
    denied(
        &b.super_call("m", "write", &[1], "[]")
            .await
            .expect_err("write"),
        "orm:write",
    );
    denied(
        &b.super_call("m", "create", &[], "[]")
            .await
            .expect_err("create"),
        "orm:create",
    );
    assert!(rec.calls().is_empty(), "nothing reached the backend");
    // a method with no capability mapping, and a private one, still reach the backend (the
    // backend pins the name to the running method)
    let err = b
        .super_call("m", "_check_x", &[1], "[]")
        .await
        .expect_err("no dispatcher");
    assert_eq!(err.kind, BoundaryErrorKind::Unsupported);
    let all = OrmBridge::new(
        Arc::new(Recorder::default()),
        caller(7, 3),
        OrmCapabilities::all(),
        Limits::default(),
    );
    let err = all
        .super_call("m", "unlink", &[1], "[]")
        .await
        .expect_err("no dispatcher");
    assert_eq!(err.kind, BoundaryErrorKind::Unsupported);
}

#[tokio::test]
async fn a_declared_pair_does_not_unlock_the_name_on_another_model() {
    let rec = Arc::new(Recorder::default());
    let pairs: std::collections::HashSet<(String, String)> =
        [("sale.order".to_owned(), "action_confirm".to_owned())]
            .into_iter()
            .collect();
    let b = OrmBridge::new(
        rec.clone(),
        caller(7, 3),
        OrmCapabilities::all(),
        Limits::default(),
    )
    .with_callable_model_methods(Arc::new(pairs));
    b.call_method("sale.order", "action_confirm", &[1], "[]")
        .await
        .expect("declared pair");
    let err = b
        .call_method("account.move", "action_confirm", &[1], "[]")
        .await
        .expect_err("same name, other model");
    assert_eq!(err.kind, BoundaryErrorKind::AccessDenied);
    assert!(
        err.message.contains("not declared callable"),
        "{}",
        err.message
    );
    assert_eq!(rec.calls().len(), 1);
}

#[tokio::test]
async fn long_log_messages_are_cut_on_a_character_boundary() {
    let (b, rec) = bridge(OrmCapabilities::NONE, Limits::default());
    // 2-byte characters: 4096 is even, so cut one in the middle with an odd prefix.
    let message = format!("a{}", "é".repeat(5000));
    b.log(LogLevel::Info, "addon::x", &message);
    let logs = rec.logs.lock().expect("lock");
    assert!(
        logs[0].len() <= 4096 && logs[0].len() > 4000,
        "{}",
        logs[0].len()
    );
    assert!(logs[0].chars().all(|c| c == 'a' || c == 'é'));
}

#[test]
fn caller_ids_must_be_positive_and_the_company_allowed() {
    assert!(matches!(
        Caller::new(0, 1, vec![1]),
        Err(HostError::Config(_))
    ));
    assert!(matches!(
        Caller::new(1, 0, vec![0]),
        Err(HostError::Config(_))
    ));
    let err = Caller::new(1, 2, vec![1, 3]).expect_err("company");
    assert!(
        err.to_string().contains("not in the allowed companies"),
        "{err}"
    );
    assert_eq!(
        Caller::new(1, 3, vec![1, 3])
            .expect("ok")
            .allowed_company_ids(),
        &[1, 3]
    );
}

#[tokio::test]
async fn set_caller_without_a_bound_bridge_fails_loudly() {
    let host = WasmHost::new().expect("host");
    let bytes = wat::parse_str("(component)").expect("component");
    let comp = host.component(&bytes).expect("compile");
    let mut store = host.store(&comp, Limits::default());
    let err = store.set_caller(caller(1, 1)).expect_err("unbound");
    assert!(err.to_string().contains("no ORM bridge"), "{err}");
}

/// their capabilities)
#[tokio::test]
async fn call_method_cannot_bypass_the_narrower_capabilities() {
    let caps = OrmCapabilities::NONE.with(OrmOp::CallMethod);
    let (b, rec) = bridge(caps, Limits::default());
    denied(
        &b.call_method("m", "unlink", &[1], "[]")
            .await
            .expect_err("unlink"),
        "orm:unlink",
    );
    denied(
        &b.call_method("m", "write", &[1], "[]")
            .await
            .expect_err("write"),
        "orm:write",
    );
    denied(
        &b.call_method("m", "create", &[], "[]")
            .await
            .expect_err("create"),
        "orm:create",
    );
    let err = b
        .call_method("m", "_flush", &[1], "[]")
        .await
        .expect_err("private");
    assert_eq!(err.kind, BoundaryErrorKind::AccessDenied);
    assert!(
        err.message.contains("method `_flush` is not callable"),
        "{}",
        err.message
    );
    assert!(rec.calls().is_empty());
    b.call_method("m", "action_confirm", &[1], "[]")
        .await
        .expect("plain method");
    assert_eq!(rec.calls().len(), 1);
}

#[tokio::test]
async fn search_limit_is_clamped_to_the_batch_limit() {
    let limits = Limits::default().with_batch_ids(2).expect("ids");
    let seen = Arc::new(Mutex::new(None));
    struct Limit(Arc<Mutex<Option<Option<u32>>>>);
    impl OrmBackend for Limit {
        fn search<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _d: &'a str,
            limit: Option<u32>,
            _o: Option<u32>,
            _s: Option<&'a str>,
        ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
            *self.0.lock().expect("lock") = Some(limit);
            Box::pin(async { Ok(vec![1]) })
        }
        fn read_columns<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _f: &'a [String],
        ) -> BackendFuture<'a, BridgeResult<RecordBatch>> {
            unreachable!()
        }
        fn create_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _v: &'a [Vec<(String, ScalarValue)>],
        ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
            unreachable!()
        }
        fn write_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _v: &'a [(String, ScalarValue)],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn write_columns<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _v: &'a [ColumnVector],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn unlink_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn call_method<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _me: &'a str,
            _i: &'a [i64],
            _a: &'a str,
        ) -> BackendFuture<'a, BridgeResult<String>> {
            unreachable!()
        }
        fn log(&self, _c: &Caller, _l: LogLevel, _t: &str, _m: &str) {}
    }
    let b = OrmBridge::new(
        Arc::new(Limit(seen.clone())),
        caller(1, 1),
        OrmCapabilities::all(),
        limits,
    );
    b.search("m", "[]", None, None, None).await.expect("none");
    assert_eq!(*seen.lock().expect("lock"), Some(Some(2)));
    b.search("m", "[]", Some(999), None, None)
        .await
        .expect("big");
    assert_eq!(*seen.lock().expect("lock"), Some(Some(2)));
    b.search("m", "[]", Some(1), None, None)
        .await
        .expect("small");
    assert_eq!(*seen.lock().expect("lock"), Some(Some(1)));
}

#[tokio::test]
async fn read_columns_returns_the_batch_for_the_requested_ids() {
    let (b, _) = bridge(OrmCapabilities::all(), Limits::default());
    b.read_columns("m", &[1, 2], &["a".into()])
        .await
        .expect("ok");
}

#[tokio::test]
async fn read_columns_with_a_dropped_field_is_an_internal_error() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    *rec.read_columns.lock().expect("lock") = Some(1);
    let err = b
        .read_columns("m", &[1, 2], &["a".into(), "b".into()])
        .await
        .expect_err("dropped column");
    assert_eq!(err.kind, BoundaryErrorKind::InternalError);
    assert!(
        err.message.contains("1 columns for 2 ids and 2 fields"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn write_columns_beyond_the_batch_limit_of_columns_is_resource_exhausted() {
    let limits = Limits::default().with_batch_ids(1).expect("ids");
    let (b, rec) = bridge(OrmCapabilities::all(), limits);
    let col = |f: &str| ColumnVector {
        field: f.into(),
        data: ColumnData::ColInt(vec![1]),
    };
    let err = b
        .write_columns("m", &[1], &[col("a"), col("b")])
        .await
        .expect_err("columns");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(err.message.contains("write columns"), "{}", err.message);
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn log_target_is_cut_like_the_message() {
    struct Target(Mutex<usize>);
    impl OrmBackend for Target {
        fn search<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _d: &'a str,
            _l: Option<u32>,
            _o: Option<u32>,
            _s: Option<&'a str>,
        ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
            unreachable!()
        }
        fn read_columns<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _f: &'a [String],
        ) -> BackendFuture<'a, BridgeResult<RecordBatch>> {
            unreachable!()
        }
        fn create_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _v: &'a [Vec<(String, ScalarValue)>],
        ) -> BackendFuture<'a, BridgeResult<Vec<i64>>> {
            unreachable!()
        }
        fn write_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _v: &'a [(String, ScalarValue)],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn write_columns<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
            _v: &'a [ColumnVector],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn unlink_batch<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _i: &'a [i64],
        ) -> BackendFuture<'a, BridgeResult<()>> {
            unreachable!()
        }
        fn call_method<'a>(
            &'a self,
            _c: &'a Caller,
            _m: &'a str,
            _me: &'a str,
            _i: &'a [i64],
            _a: &'a str,
        ) -> BackendFuture<'a, BridgeResult<String>> {
            unreachable!()
        }
        fn log(&self, _c: &Caller, _l: LogLevel, target: &str, _m: &str) {
            *self.0.lock().expect("lock") = target.len();
        }
    }
    let backend = Arc::new(Target(Mutex::new(0)));
    let b = OrmBridge::new(
        backend.clone(),
        caller(1, 1),
        OrmCapabilities::NONE,
        Limits::default(),
    );
    b.log(LogLevel::Info, &"t".repeat(10_000), "m");
    assert_eq!(*backend.0.lock().expect("lock"), 256);
}

fn val(f: &str, v: ScalarValue) -> Vec<(String, ScalarValue)> {
    vec![(f.to_owned(), v)]
}

fn x2m(cmd: X2manyCommand) -> Vec<(String, ScalarValue)> {
    val("line_ids", ScalarValue::ValX2many(vec![cmd]))
}

#[tokio::test]
async fn x2many_commands_that_remove_records_need_the_unlink_capability() {
    let write_only = OrmCapabilities::NONE.with(OrmOp::Write).with(OrmOp::Create);
    let (b, rec) = bridge(write_only, Limits::default());
    for cmd in [
        X2manyCommand::Delete(5),
        X2manyCommand::Unlink(5),
        X2manyCommand::Clear,
        X2manyCommand::Set(vec![5]),
    ] {
        denied(
            &b.write_batch("m", &[1], &x2m(cmd))
                .await
                .expect_err("write"),
            "orm:unlink",
        );
    }
    // On create nothing is linked yet: only `delete` removes.
    denied(
        &b.create_batch("m", &[x2m(X2manyCommand::Delete(5))])
            .await
            .expect_err("create delete"),
        "orm:unlink",
    );
    let col = ColumnVector {
        field: "line_ids".into(),
        data: ColumnData::ColIds(vec![vec![5]]),
    };
    denied(
        &b.write_columns("m", &[1], std::slice::from_ref(&col))
            .await
            .expect_err("columns"),
        "orm:unlink",
    );
    assert!(rec.calls().is_empty(), "no denied call reaches the ORM");
    b.write_batch("m", &[1], &x2m(X2manyCommand::Link(5)))
        .await
        .expect("link is allowed");
    b.create_batch("m", &[x2m(X2manyCommand::Set(vec![5]))])
        .await
        .expect("set on create is allowed");
    let (b, rec) = bridge(write_only.with(OrmOp::Unlink), Limits::default());
    b.write_batch("m", &[1], &x2m(X2manyCommand::Delete(5)))
        .await
        .expect("allowed with unlink");
    b.write_columns("m", &[1], &[col]).await.expect("columns");
    assert_eq!(rec.calls().len(), 2);
}

#[tokio::test]
async fn call_arguments_cannot_carry_identity_or_authority_keys() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    for args in [
        r#"{"uid": 1}"#,
        r#"{"su": true}"#,
        r#"{"kwargs": {"company_id": 2}}"#,
        r#"{"context": {"allowed_company_ids": [1, 2]}}"#,
        r#"{"kwargs": {"context": {"force_company": 2}}}"#,
        r#"[1, {"context": {"UID": 1}}]"#,
        r#"[[{"context": {"su": 1}}]]"#,
    ] {
        let err = b
            .call_method("m", "action_confirm", &[1], args)
            .await
            .expect_err(args);
        assert_eq!(err.kind, BoundaryErrorKind::AccessDenied, "{args}");
        assert!(err.message.contains("identity key"), "{}", err.message);
    }
    // A harmless context key and a data value named like a key (not a method argument) pass.
    b.call_method(
        "m",
        "action_confirm",
        &[1],
        r#"{"context": {"lang": "fr_FR"}}"#,
    )
    .await
    .expect("lang");
    b.call_method(
        "m",
        "action_confirm",
        &[1],
        r#"[{"vals": {"company_id": 2}}]"#,
    )
    .await
    .expect("nested data value");
    assert_eq!(rec.calls().len(), 2);
    let err = b
        .call_method("m", "action_confirm", &[1], "{not json")
        .await
        .expect_err("json");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    for method in [
        "sudo",
        "with_user",
        "with_company",
        "with_context",
        "with_env",
        "With_Context",
        "env",
    ] {
        let err = b
            .call_method("m", method, &[1], "[]")
            .await
            .expect_err(method);
        assert_eq!(err.kind, BoundaryErrorKind::AccessDenied, "{method}");
        assert!(
            err.message.contains("identity or the environment"),
            "{}",
            err.message
        );
    }
    assert_eq!(rec.calls().len(), 2);
}

#[tokio::test]
async fn call_method_denies_or_gates_every_mutator_family() {
    let call_only = OrmCapabilities::NONE.with(OrmOp::CallMethod);
    let (b, rec) = bridge(call_only, Limits::default());
    for (method, cap) in [
        ("copy", "orm:create"),
        ("copy_data", "orm:create"),
        ("name_create", "orm:create"),
        ("toggle_active", "orm:write"),
        ("action_archive", "orm:write"),
        ("action_unarchive", "orm:write"),
        ("update", "orm:write"),
        ("import_data", "orm:create"),
        ("load", "orm:create"),
        ("Write", "orm:write"),
    ] {
        denied(
            &b.call_method("m", method, &[1], "[]")
                .await
                .expect_err(method),
            cap,
        );
    }
    for method in [
        "write_multi",
        "create_multi",
        "flush_model",
        "invalidate_all",
        "_x",
        "__init__",
    ] {
        let err = b
            .call_method("m", method, &[1], "[]")
            .await
            .expect_err(method);
        assert_eq!(err.kind, BoundaryErrorKind::AccessDenied, "{method}");
    }
    let err = b
        .call_method("m", "a b", &[1], "[]")
        .await
        .expect_err("grammar");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert!(rec.calls().is_empty());
    // Value-carrying mutators also need unlink: their JSON may hold x2many remove commands.
    let (b, _) = bridge(
        OrmCapabilities::NONE
            .with(OrmOp::CallMethod)
            .with(OrmOp::Write),
        Limits::default(),
    );
    denied(
        &b.call_method("m", "write", &[1], "[]")
            .await
            .expect_err("write needs unlink"),
        "orm:unlink",
    );
}

#[tokio::test]
async fn a_declared_method_list_is_the_only_thing_call_method_reaches() {
    let rec = Arc::new(Recorder::default());
    let b = OrmBridge::new(
        rec.clone(),
        caller(7, 3),
        OrmCapabilities::all(),
        Limits::default(),
    )
    .with_callable_methods(["action_confirm", "sudo"]);
    b.call_method("m", "action_confirm", &[1], "[]")
        .await
        .expect("declared");
    let err = b
        .call_method("m", "action_done", &[1], "[]")
        .await
        .expect_err("undeclared");
    assert_eq!(err.kind, BoundaryErrorKind::AccessDenied);
    assert!(
        err.message.contains("not declared callable"),
        "{}",
        err.message
    );
    // Declaring a method does not lift the built-in floor.
    let err = b
        .call_method("m", "sudo", &[1], "[]")
        .await
        .expect_err("sudo");
    assert!(
        err.message.contains("identity or the environment"),
        "{}",
        err.message
    );
    assert_eq!(rec.calls().len(), 1);
}

#[tokio::test]
async fn order_offset_fields_and_names_are_bounded_at_the_limit_and_one_above() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    let s = |order: &str| {
        let b = b.clone();
        let order = order.to_owned();
        async move { b.search("m", "[]", None, None, Some(&order)).await }
    };
    // Grammar.
    for ok in ["name", "name desc", "name ASC, id desc", "a,a,a,a,a,a,a,a"] {
        s(ok).await.expect(ok);
    }
    for bad in [
        "",
        "name,",
        "name sideways",
        "name desc nulls first",
        "id; drop table x",
        "Name",
        "1id",
        "a.b",
        "name desc, ",
        "a,a,a,a,a,a,a,a,a",
    ] {
        s(bad).await.expect_err(bad);
    }
    let two = |n: usize| format!("{},{}", "a".repeat(127), "b".repeat(n));
    s(&two(128)).await.expect("256 bytes");
    let err = s(&two(129)).await.expect_err("257 bytes");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    // Offset.
    b.search("m", "[]", None, Some(erp_wasm_host::MAX_OFFSET), None)
        .await
        .expect("max offset");
    let err = b
        .search("m", "[]", None, Some(erp_wasm_host::MAX_OFFSET + 1), None)
        .await
        .expect_err("offset");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    // Fields.
    let fields = |n: usize| (0..n).map(|i| format!("f{i}")).collect::<Vec<_>>();
    b.read_columns("m", &[1], &fields(erp_wasm_host::MAX_FIELDS))
        .await
        .expect("max fields");
    let err = b
        .read_columns("m", &[1], &fields(erp_wasm_host::MAX_FIELDS + 1))
        .await
        .expect_err("fields");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    // Name lengths.
    let long = |n: usize| "a".repeat(n);
    b.read_columns("m", &[1], &[long(erp_wasm_host::MAX_NAME_BYTES)])
        .await
        .expect("field at the limit");
    let err = b
        .read_columns("m", &[1], &[long(erp_wasm_host::MAX_NAME_BYTES + 1)])
        .await
        .expect_err("field");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    b.search(&long(erp_wasm_host::MAX_NAME_BYTES), "[]", None, None, None)
        .await
        .expect("model at the limit");
    let err = b
        .search(
            &long(erp_wasm_host::MAX_NAME_BYTES + 1),
            "[]",
            None,
            None,
            None,
        )
        .await
        .expect_err("model");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    let err = b
        .search("Sale Order", "[]", None, None, None)
        .await
        .expect_err("model grammar");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    let err = b
        .call_method("m", &long(erp_wasm_host::MAX_NAME_BYTES + 1), &[1], "[]")
        .await
        .expect_err("method");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    let err = b
        .write_batch("m", &[1], &val("Bad-Field", ScalarValue::ValNull))
        .await
        .expect_err("value field");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert_eq!(rec.calls().len(), 9);
}

#[tokio::test]
async fn a_read_answer_over_the_payload_cap_is_refused_at_the_cap_plus_one() {
    // 4 ids (32 bytes) + one int column of 4 values (64 bytes) + the field name (2 bytes).
    let ids = [1, 2, 3, 4];
    let fields = ["f0".to_owned()];
    let (b, _) = bridge(
        OrmCapabilities::all(),
        Limits::default().with_payload_bytes(98).expect("cap"),
    );
    b.read_columns("m", &ids, &fields)
        .await
        .expect("at the cap");
    let (b, _) = bridge(
        OrmCapabilities::all(),
        Limits::default().with_payload_bytes(97).expect("cap"),
    );
    let err = b
        .read_columns("m", &ids, &fields)
        .await
        .expect_err("over the cap");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(err.message.contains("read answer"), "{}", err.message);
}

#[tokio::test]
async fn x2many_set_ids_count_toward_the_payload() {
    let limits = Limits::default().with_payload_bytes(100).expect("cap");
    let (b, rec) = bridge(OrmCapabilities::all(), limits);
    let err = b
        .write_batch("m", &[1], &x2m(X2manyCommand::Set((1..=100).collect())))
        .await
        .expect_err("big set");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn a_post_effect_check_failure_is_an_error_and_marks_the_call_for_rollback() {
    let limits = Limits::default().with_payload_bytes(16).expect("cap");
    let (b, rec) = bridge(OrmCapabilities::all(), limits);
    *rec.call_answer.lock().expect("lock") = Some("x".repeat(17));
    assert!(!b.take_tainted());
    let err = b
        .call_method("m", "action_confirm", &[1], "[]")
        .await
        .expect_err("result over the cap");
    assert_eq!(err.kind, BoundaryErrorKind::ResourceExhausted);
    assert_eq!(rec.calls().len(), 1, "the method ran before the check");
    assert!(b.take_tainted(), "the dispatcher must roll the call back");
    assert!(!b.take_tainted(), "the mark is read once");
    // At the cap the call succeeds and nothing is marked.
    *rec.call_answer.lock().expect("lock") = Some("x".repeat(16));
    b.call_method("m", "action_confirm", &[1], "[]")
        .await
        .expect("at the cap");
    assert!(!b.take_tainted());
    // A create that returned the wrong count is marked too.
    *rec.created.lock().expect("lock") = Some(1);
    b.create_batch("m", &[vec![], vec![]])
        .await
        .expect_err("count");
    assert!(b.take_tainted());
    // Checks that run before the effect never mark: the backend is not reached.
    let before = rec.calls().len();
    b.unlink_batch("m", &[0]).await.expect_err("bad id");
    assert_eq!(rec.calls().len(), before);
    assert!(!b.take_tainted());
}

/// review r1 M5)
#[tokio::test]
async fn nested_call_method_stops_at_the_depth_limit_with_a_typed_error() {
    let limits = Limits::default().with_stack_depth(3).expect("depth");
    let (b, rec) = bridge(OrmCapabilities::all(), limits);
    *rec.nest.lock().expect("lock") = Some(b.clone());
    b.call_method("m", "ping", &[], "[]").await.expect("outer");
    assert_eq!(*rec.nested_runs.lock().expect("lock"), 3, "depth 1..=3 ran");
    let stop = rec
        .nested_stop
        .lock()
        .expect("lock")
        .clone()
        .expect("stopped");
    assert_eq!(stop.kind, BoundaryErrorKind::ResourceExhausted);
    assert!(
        stop.message.contains("depth limit of 3"),
        "{}",
        stop.message
    );
    // The counter unwinds: a later top-level call works again.
    *rec.nest.lock().expect("lock") = None;
    b.call_method("m", "ping", &[], "[]").await.expect("after");
}

#[tokio::test]
async fn ids_must_be_positive_and_unique() {
    let (b, rec) = bridge(OrmCapabilities::all(), Limits::default());
    for ids in [&[0_i64][..], &[-1][..], &[1, 2, 1][..]] {
        for result in [
            b.read_columns("m", ids, &["a".into()]).await.map(|_| ()),
            b.write_batch("m", ids, &[]).await,
            b.write_columns("m", ids, &[]).await,
            b.unlink_batch("m", ids).await,
            b.call_method("m", "ping", ids, "[]").await.map(|_| ()),
        ] {
            let err = result.expect_err("ids");
            assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument, "{ids:?}");
        }
    }
    let dup = [
        ColumnVector {
            field: "a".into(),
            data: ColumnData::ColInt(vec![1]),
        },
        ColumnVector {
            field: "a".into(),
            data: ColumnData::ColInt(vec![2]),
        },
    ];
    let err = b.write_columns("m", &[1], &dup).await.expect_err("dup");
    assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
    assert!(rec.calls().is_empty());
}

#[tokio::test]
async fn log_events_are_sanitised_and_bounded_without_a_silent_drop() {
    let (b, rec) = bridge(OrmCapabilities::NONE, Limits::default());
    b.log(LogLevel::Info, "addon::x\nfake", "line one\nFAKE ERROR");
    assert_eq!(rec.logs.lock().expect("lock")[0], "line one FAKE ERROR");
    // 10,000 events are forwarded in all; the 10,001st becomes one warning, the rest are counted.
    for _ in 1..10_000 {
        b.log(LogLevel::Info, "t", "m");
    }
    assert_eq!(b.dropped_logs(), 0);
    b.log(LogLevel::Info, "t", "m");
    b.log(LogLevel::Info, "t", "m");
    b.log(LogLevel::Info, "t", "m");
    assert_eq!(b.dropped_logs(), 3);
    let logs = rec.logs.lock().expect("lock");
    assert_eq!(logs.len(), 10_001);
    assert!(
        logs[10_000].contains("budget exhausted"),
        "{}",
        logs[10_000]
    );
}

#[tokio::test]
async fn a_plain_id_list_on_write_needs_unlink_like_set_and_col_ids() {
    let write_only = OrmCapabilities::NONE.with(OrmOp::Write).with(OrmOp::Create);
    let (b, rec) = bridge(write_only, Limits::default());
    denied(
        &b.write_batch("m", &[1], &val("line_ids", ScalarValue::ValIds(vec![5])))
            .await
            .expect_err("write ids"),
        "orm:unlink",
    );
    assert!(rec.calls().is_empty());
    b.create_batch("m", &[val("line_ids", ScalarValue::ValIds(vec![5]))])
        .await
        .expect("nothing is linked yet on create");
    let (b, _) = bridge(write_only.with(OrmOp::Unlink), Limits::default());
    b.write_batch("m", &[1], &val("line_ids", ScalarValue::ValIds(vec![5])))
        .await
        .expect("allowed with unlink");
}

/// default, lead security pass 2)
#[tokio::test]
async fn read_family_methods_need_read_and_undeclared_methods_are_refused_by_default() {
    let (b, rec) = bridge(
        OrmCapabilities::NONE.with(OrmOp::CallMethod),
        Limits::default(),
    );
    for method in [
        "read",
        "search",
        "search_read",
        "search_count",
        "read_group",
        "name_search",
        "name_get",
        "export_data",
        "fields_get",
        "web_read",
        "web_search_read",
    ] {
        denied(
            &b.call_method("m", method, &[1], "[]")
                .await
                .expect_err(method),
            "orm:read",
        );
    }
    // `web_save` writes through a read-looking name.
    denied(
        &b.call_method("m", "web_save", &[1], "[]")
            .await
            .expect_err("web_save"),
        "orm:read",
    );
    assert!(rec.calls().is_empty());
    let (b, _) = bridge(OrmCapabilities::all(), Limits::default());
    b.call_method("m", "read", &[1], "[]").await.expect("read");

    // A fresh bridge declares no method: even a harmless name is refused until the host declares it.
    let rec = Arc::new(Recorder::default());
    let b = OrmBridge::new(
        rec.clone(),
        caller(7, 3),
        OrmCapabilities::all(),
        Limits::default(),
    );
    let err = b
        .call_method("m", "action_confirm", &[1], "[]")
        .await
        .expect_err("undeclared");
    assert!(
        err.message.contains("not declared callable"),
        "{}",
        err.message
    );
    assert!(rec.calls().is_empty());
}
