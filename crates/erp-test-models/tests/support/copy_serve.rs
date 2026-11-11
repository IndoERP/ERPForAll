//! Private HTTP/RPC copy fixture. Both modes use identical framing, binders and serializers.
//! SQL overrides implement only the bounded two-field fixture, never production behavior.
#![allow(dead_code, reason = "shared test and diagnostic example support")]

use bytes::Bytes;
use erp_fields::{Result, Value, ValueMap};
use erp_orm::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super};
use erp_orm::{Principal, PrincipalBuilder, Records, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, StaticValue};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcService};
use erp_testkit::{
    Harness,
    serve::{GaugedIo, RawClient, ServeContext, ServeProbe},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Value as Json, json};
use std::{
    any::Any,
    io,
    sync::{Arc, mpsc},
    time::Duration,
};

const LIMIT: usize = 1024 * 1024;
const MODEL: &str = "test_copy_serve.item";
static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_copy_serve",
    depends: &["base"],
    exact_only: false,
};

fn error(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(e)
}

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Erp,
    Floor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Copy,
    Data,
    Name,
    Archive,
    Unarchive,
}
pub const OPERATIONS: [Operation; 5] = [
    Operation::Copy,
    Operation::Data,
    Operation::Name,
    Operation::Archive,
    Operation::Unarchive,
];
impl Operation {
    pub fn method(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Data => "copy_data",
            Self::Name => "name_create",
            Self::Archive => "action_archive",
            Self::Unarchive => "action_unarchive",
        }
    }
    fn initial_active(self) -> bool {
        self != Self::Unarchive
    }
    pub fn request(self, rows: usize) -> io::Result<Vec<u8>> {
        let body = if self == Self::Name {
            json!({"name": "Created"})
        } else {
            json!({"ids": (1..=rows).collect::<Vec<_>>()})
        };
        Ok(frame(
            &format!("/json/2/{MODEL}/{}", self.method()),
            &serde_json::to_vec(&body).map_err(error)?,
        ))
    }
    pub fn expected(self, rows: usize) -> Json {
        match self {
            Self::Copy => json!((rows + 1..=rows * 2).collect::<Vec<_>>()),
            Self::Data => json!(
                (1..=rows)
                    .map(|i| json!({"name": format!("Name {i}"), "active": true}))
                    .collect::<Vec<_>>()
            ),
            Self::Name => json!([rows + 1, "Created"]),
            Self::Archive | Self::Unarchive => Json::Null,
        }
    }
}

fn frame(path: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("POST {path} HTTP/1.1\r\nhost: copy\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

// Only registered in the floor's private registry. Core RPC binders and effects are unchanged.
macro_rules! implementation {
    ($adapter:ident, $entry:ident, $meta:ident, $handler:ident, $args:ty, $ret:ty, $method:literal) => {
        fn $adapter<'a>(
            tx: &'a mut Transaction,
            records: &'a Records,
            args: $args,
            _sup: Super<$args, $ret>,
        ) -> BoxFuture<'a, Result<$ret>> {
            Box::pin($handler(tx, records, args))
        }
        fn $entry() -> &'static (dyn Any + Send + Sync) {
            static F: MethodFn<$args, $ret> = $adapter;
            &F
        }
        static $meta: MethodImpl = MethodImpl {
            addon: "test_copy_serve",
            model: MODEL,
            method: $method,
            kind: ImplKind::Override,
            sig_name: std::any::type_name::<MethodFn<$args, $ret>>,
            entry: $entry,
            file: file!(),
            line: line!(),
        };
    };
}

async fn data_floor(
    tx: &mut Transaction,
    records: &Records,
    args: (Option<ValueMap>,),
) -> Result<Vec<Option<ValueMap>>> {
    if args.0.is_some() {
        return Err(erp_fields::Error::validation(
            "copy floor supports only absent defaults",
        ));
    }
    let rows = tx
        .query(
            "SELECT name, active FROM test_copy_serve_item WHERE id = ANY($1) ORDER BY id",
            &[&records.ids()],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            Some(ValueMap::from([
                ("name".to_owned(), Value::str(row.get::<_, &str>(0))),
                ("active".to_owned(), Value::Bool(row.get(1))),
            ]))
        })
        .collect())
}
async fn copy_floor(
    tx: &mut Transaction,
    records: &Records,
    args: (Option<ValueMap>,),
) -> Result<Records> {
    if args.0.is_some() {
        return Err(erp_fields::Error::validation(
            "copy floor supports only absent defaults",
        ));
    }
    let rows = tx.query("INSERT INTO test_copy_serve_item (name, active) SELECT name, active FROM test_copy_serve_item WHERE id = ANY($1) ORDER BY id RETURNING id", &[&records.ids()]).await?;
    records.browse(rows.iter().map(|row| row.get::<_, i64>(0)))
}
async fn name_floor(
    tx: &mut Transaction,
    _records: &Records,
    args: (String,),
) -> Result<(i64, Arc<str>)> {
    let rows = tx
        .query(
            "INSERT INTO test_copy_serve_item (name, active) VALUES ($1, true) RETURNING id, name",
            &[&args.0],
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(erp_fields::Error::programming(
            "name floor expected one row",
        ));
    };
    Ok((row.get(0), Arc::from(row.get::<_, &str>(1))))
}
async fn archive_floor(tx: &mut Transaction, records: &Records, (): ()) -> Result<()> {
    tx.execute(
        "UPDATE test_copy_serve_item SET active = false WHERE id = ANY($1) AND active = true",
        &[&records.ids()],
    )
    .await?;
    Ok(())
}
async fn unarchive_floor(tx: &mut Transaction, records: &Records, (): ()) -> Result<()> {
    tx.execute(
        "UPDATE test_copy_serve_item SET active = true WHERE id = ANY($1) AND active = false",
        &[&records.ids()],
    )
    .await?;
    Ok(())
}
implementation!(
    data_impl,
    data_entry,
    DATA,
    data_floor,
    (Option<ValueMap>,),
    Vec<Option<ValueMap>>,
    "copy_data"
);
implementation!(
    copy_impl,
    copy_entry,
    COPY,
    copy_floor,
    (Option<ValueMap>,),
    Records,
    "copy"
);
implementation!(
    name_impl,
    name_entry,
    NAME,
    name_floor,
    (String,),
    (i64, Arc<str>),
    "name_create"
);
implementation!(
    archive_impl,
    archive_entry,
    ARCHIVE,
    archive_floor,
    (),
    (),
    "action_archive"
);
implementation!(
    unarchive_impl,
    unarchive_entry,
    UNARCHIVE,
    unarchive_floor,
    (),
    (),
    "action_unarchive"
);

struct Host;
impl RpcHost for Host {
    fn language_active(&self, _code: &str) -> Result<bool> {
        Ok(false)
    }
    fn has_capability(&self, _p: &Principal, _c: &erp_orm::Capability) -> Result<bool> {
        Ok(false)
    }
}

async fn reset(h: &Harness, operation: Operation, rows: usize) -> io::Result<()> {
    let mut tx = h.begin().await.map_err(error)?;
    tx.batch("TRUNCATE test_copy_serve_item RESTART IDENTITY")
        .await
        .map_err(error)?;
    let count = i64::try_from(rows).map_err(error)?;
    tx.execute("INSERT INTO test_copy_serve_item (name, active) SELECT 'Name ' || i, $2 FROM generate_series(1, $1::int8) AS i ORDER BY i", &[&count, &operation.initial_active()]).await.map_err(error)?;
    tx.commit().await.map_err(error)
}

async fn state(h: &Harness) -> io::Result<Json> {
    let mut tx = h.begin().await.map_err(error)?;
    let values = tx
        .query(
            "SELECT id, name, active FROM test_copy_serve_item ORDER BY id",
            &[],
        )
        .await
        .map_err(error)?;
    let result = json!(
        values
            .iter()
            .map(|r| json!([r.get::<_, i64>(0), r.get::<_, &str>(1), r.get::<_, bool>(2)]))
            .collect::<Vec<_>>()
    );
    tx.commit().await.map_err(error)?;
    Ok(result)
}

async fn answer(
    req: Request<Incoming>,
    h: &Harness,
    service: &RpcService,
    operation: Operation,
    rows: usize,
    principal: Principal,
) -> io::Result<Response<Full<Bytes>>> {
    if req.method() != hyper::Method::POST {
        return Err(error("fixture requires POST"));
    }
    let path = req.uri().path().to_owned();
    let body = Limited::new(req.into_body(), LIMIT)
        .collect()
        .await
        .map_err(error)?
        .to_bytes();
    let (status, bytes, statements) = match path.as_str() {
        "/__copy/reset" => {
            if body != b"{}"[..] {
                return Err(error("invalid reset body"));
            }
            reset(h, operation, rows).await?;
            (200, b"{}".to_vec(), 0)
        }
        "/__copy/state" => {
            if body != b"{}"[..] {
                return Err(error("invalid state body"));
            }
            (200, serde_json::to_vec(&state(h).await?).map_err(error)?, 0)
        }
        "/__copy/counters" => {
            if body != b"{}"[..] {
                return Err(error("invalid counter body"));
            }
            if tokio::runtime::Handle::current().runtime_flavor()
                != tokio::runtime::RuntimeFlavor::CurrentThread
            {
                return Err(error("driver snapshots require a current-thread runtime"));
            }
            // Same singleton pool as RpcService. Checkout and JSON serialization occur
            // outside the allocation window and issue no SQL.
            let connection = h.db().pool().get().await.map_err(error)?;
            let counts = json!({
                "connection": format!("{:?}", erp_testkit::deadpool_postgres::Object::id(&connection)),
                "delivered": connection.response_batches(),
                "parked": connection.parked_batches(),
            });
            (200, serde_json::to_vec(&counts).map_err(error)?, 0)
        }
        _ => {
            if path != format!("/json/2/{MODEL}/{}", operation.method()) {
                return Err(error("unexpected fixture route"));
            }
            let response = service
                .call(RpcCall::json2(
                    &path,
                    body.to_vec(),
                    Caller::Principal(principal),
                ))
                .await;
            (
                response.status(),
                response.body().to_vec(),
                response.statements(),
            )
        }
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", bytes.len())
        .header("x-copy-statements", statements)
        .body(Full::new(Bytes::from(bytes)))
        .map_err(error)
}

pub struct Served {
    pub probe: ServeProbe,
    shutdown: tokio::sync::oneshot::Sender<()>,
    stopped: mpsc::Receiver<()>,
    operation: Operation,
    rows: usize,
}
impl Served {
    pub fn client(&self) -> io::Result<RawClient> {
        RawClient::connect(self.probe.addr(), self.operation.request(self.rows)?, LIMIT)
            .map_err(error)
    }
    pub fn reset(&self, client: &mut RawClient) -> io::Result<()> {
        client
            .set_request(&frame("/__copy/reset", b"{}"))
            .map_err(error)?;
        let response = client.send().map_err(error)?;
        if response.status() != 200 || response.content() != Some(&b"{}"[..]) {
            return Err(error("reset failed"));
        }
        client
            .set_request(&self.operation.request(self.rows)?)
            .map_err(error)
    }
    pub fn state(&self, client: &mut RawClient) -> io::Result<Json> {
        client
            .set_request(&frame("/__copy/state", b"{}"))
            .map_err(error)?;
        let response = client.send().map_err(error)?;
        if response.status() != 200 {
            return Err(error("state read failed"));
        }
        let state = serde_json::from_slice(
            response
                .content()
                .ok_or_else(|| error("state body missing"))?,
        )
        .map_err(error)?;
        client
            .set_request(&self.operation.request(self.rows)?)
            .map_err(error)?;
        Ok(state)
    }
    pub fn counters(&self, client: &mut RawClient) -> io::Result<Json> {
        client
            .set_request(&frame("/__copy/counters", b"{}"))
            .map_err(error)?;
        let response = client.send().map_err(error)?;
        if response.status() != 200 {
            return Err(error("counter read failed"));
        }
        let counters = serde_json::from_slice(
            response
                .content()
                .ok_or_else(|| error("counter body missing"))?,
        )
        .map_err(error)?;
        client
            .set_request(&self.operation.request(self.rows)?)
            .map_err(error)?;
        Ok(counters)
    }
    pub fn stop(self) -> io::Result<()> {
        self.shutdown
            .send(())
            .map_err(|()| error("shutdown receiver missing"))?;
        self.stopped
            .recv_timeout(Duration::from_secs(60))
            .map_err(error)?;
        self.probe.stop().map_err(error)
    }
}

pub fn start(mode: Mode, operation: Operation, rows: usize) -> io::Result<Served> {
    if !(1..=1_000).contains(&rows) || (operation == Operation::Name && rows != 1) {
        return Err(error(
            "fixture rows must be 1..=1000; name_create is singleton",
        ));
    }
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let probe = ServeProbe::start(move |ctx: ServeContext| async move {
        let mut builder = Harness::builder([&erp_test_models::base_users::ADDON, &ADDON]).with_decls([
            ModelDecl::new("test_copy_serve", MODEL).log_access(false).fields(const { &[FieldDecl::char("name"), FieldDecl::boolean("active").default(StaticValue::Bool(true))] })
        ]);
        if matches!(mode, Mode::Floor) { builder = builder.with_impls([&DATA, &COPY, &NAME, &ARCHIVE, &UNARCHIVE]); }
        let h = builder.build().await.map_err(error)?;
        h.db().pool().resize(1);
        let initialized = async {
            let mut tx = h.begin().await.map_err(error)?;
            tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'copy probe', 'copy_probe', true)").await.map_err(error)?;
            tx.commit().await.map_err(error)?;
            reset(&h, operation, rows).await?;
            PrincipalBuilder::user(UserId::new(2).map_err(error)?).build().map_err(error)
        }.await;
        let principal = match initialized { Ok(p) => p, Err(e) => { h.cleanup().await.map_err(error)?; return Err(e); } };
        let service = RpcService::new(h.orm().clone(), Host, RpcConfig::default());
        ready_tx.send(()).map_err(error)?;
        let connections = async {
            loop {
                let (stream, _) = ctx.listener.accept().await?;
                stream.set_nodelay(true)?;
                let io = TokioIo::new(GaugedIo::new(stream, &ctx.connections));
                let connection = http1::Builder::new().serve_connection(io, service_fn(|req| answer(req, &h, &service, operation, rows, principal.clone())));
                connection.await.map_err(error)?;
            }
            #[allow(unreachable_code, reason = "loop exits by error or explicit shutdown")]
            Ok::<(), io::Error>(())
        };
        let outcome = tokio::select! { result = connections => result, stopped = shutdown_rx => stopped.map_err(error) };
        drop(service);
        let cleanup = h.cleanup().await.map_err(error);
        stopped_tx.send(()).map_err(error)?;
        match (outcome, cleanup) { (Ok(()), Ok(())) => Ok(()), (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e), (Err(e), Err(c)) => Err(error(format!("{e}; cleanup: {c}"))) }
    }).map_err(error)?;
    if let Err(e) = ready_rx.recv_timeout(Duration::from_secs(60)) {
        return match probe.stop() {
            Ok(()) => Err(error(e)),
            Err(server) => Err(error(format!("{e}; server: {server}"))),
        };
    }
    Ok(Served {
        probe,
        shutdown,
        stopped: stopped_rx,
        operation,
        rows,
    })
}
