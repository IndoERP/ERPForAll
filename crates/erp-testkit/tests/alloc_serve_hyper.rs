//! Stock hyper 1.12 measured by the served-path harness (PERF.SERVE-HARNESS, perf-serve §3
//! "First measurements", recorded in perf-serve §12;, G6 items 3–5).
//!
//! Two services, as the design names them: a `service_fn` returning a 4 KiB static body and a
//! JSON echo of a 2 KiB body, each with hyper's defaults (no timer) and with a Tokio timer and a
//! header read timeout. "Stock" is the vendored hyper without `buffer_provider`: the hook is
//! opt-in, so these figures are released hyper's (they equal the figures taken against the
//! crates.io build before the patch, perf-serve §12). Whole-process counts (dhat). Run under
//! nextest (one process per test).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support_hyper;

use erp_testkit::serve::{self, GaugedIo, RawClient};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use support_hyper::{
    HTTP_BLOCKS_CEILING, IDLE_GATE_BYTES, RESPONSE_CAPACITY, Service, Setup, per_idle, per_request,
    request_for, start,
};

/// Stock hyper meets the per-request ceiling: the measured cost is exact (the same every request)
/// and at most G6's HTTP ceiling.
///
/// On macOS the echo rows cost one block more (7 and 8, the latter at the ceiling), on every
/// request: hyper creates a body channel per request with a body, and its `std::sync::Mutex` boxes
/// a pthread mutex on first lock on macOS (64 B; Linux's futex mutex allocates nothing), found with
/// dhat backtraces (review F6). The request arrives in one read on
/// both platforms (`stock_hyper_reads_per_request`), so the count does not depend on timing.
fn assert_within_http_ceiling(setup: &Setup) {
    let cost = per_request(setup);
    let blocks = cost
        .exact_blocks_per_request()
        .expect("the same cost for every request");
    assert!(
        blocks <= HTTP_BLOCKS_CEILING,
        "{blocks} blocks per request over the HTTP ceiling of {HTTP_BLOCKS_CEILING}"
    );
}

/// Stock hyper misses the idle gate (verdict, perf-serve §12): it keeps an 8 KiB read
/// buffer and an 8 KiB head buffer per connection.
/// While this
/// holds, `erp-server` needs the provider hook; a hyper upgrade that makes this test fail reopens
/// the D4 decision.
fn assert_misses_the_idle_gate(setup: &Setup) {
    let idle = per_idle(setup);
    let bytes = idle.bytes_per_connection();
    assert!(
        bytes > IDLE_GATE_BYTES,
        "stock hyper holds {bytes} B per idle connection: within the {IDLE_GATE_BYTES} B gate, \
         so the D4 verdict (provider hook needed) must be revisited"
    );
}

#[global_allocator]
static ALLOC: erp_testkit::alloc::Alloc = erp_testkit::alloc::Alloc;

erp_testkit::alloc::harness!(
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_static_4k_per_request,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_static_4k_per_request_with_timer,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_json_echo_per_request,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_json_echo_per_request_with_timer,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_static_4k_idle_connection_misses_the_gate,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_json_echo_idle_connection_with_timer_misses_the_gate,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_reads_per_request,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    stock_hyper_connection_future_size,
    stock_hyper_services_return_bodies,
);

fn stock_hyper_static_4k_per_request() {
    assert_within_http_ceiling(&Setup::stock(Service::Static4k, false));
}

fn stock_hyper_static_4k_per_request_with_timer() {
    assert_within_http_ceiling(&Setup::stock(Service::Static4k, true));
}

fn stock_hyper_json_echo_per_request() {
    assert_within_http_ceiling(&Setup::stock(Service::JsonEcho, false));
}

fn stock_hyper_json_echo_per_request_with_timer() {
    assert_within_http_ceiling(&Setup::stock(Service::JsonEcho, true));
}

fn stock_hyper_static_4k_idle_connection_misses_the_gate() {
    assert_misses_the_idle_gate(&Setup::stock(Service::Static4k, false));
}

fn stock_hyper_json_echo_idle_connection_with_timer_misses_the_gate() {
    assert_misses_the_idle_gate(&Setup::stock(Service::JsonEcho, true));
}

/// How many reads the server makes per request: the client writes each request with one
/// `write_all`, so one read per request unless the transport splits the write. Printed for the
/// record (perf-serve §12.1, review F6); the echo request is 2,189 bytes.
fn reads_per_request(service: Service) -> f64 {
    const REQUESTS: u64 = 200;
    let setup = Setup::stock(service, false).counting_reads();
    let counter = setup.reads.clone().expect("counting");
    let probe = start(&setup);
    let mut client =
        RawClient::connect(probe.addr(), request_for(service), RESPONSE_CAPACITY).expect("client");
    for _ in 0..16 {
        client.send().expect("warm-up");
    }
    probe.settle().expect("settles");
    let (before, _, _) = counter.snapshot();
    for _ in 0..REQUESTS {
        client.send().expect("response");
    }
    probe.settle().expect("settles");
    let (after, _, largest) = counter.snapshot();
    drop(client);
    probe.stop().expect("server stops cleanly");
    #[allow(clippy::cast_precision_loss, reason = "small counts")]
    let per_request = (after - before) as f64 / REQUESTS as f64;
    eprintln!(
        "SERVE-HARNESS-RESULT reads_per_request {service:?}: {per_request:.3} server reads per \
         request (largest read {largest} B, request {} B)",
        request_for(service).len()
    );
    per_request
}

fn stock_hyper_reads_per_request() {
    for service in [Service::Static4k, Service::JsonEcho] {
        assert!(reads_per_request(service) >= 1.0);
    }
}

/// The connection future as hyper builds it (not polled), over a plain Tokio stream and over the
/// harness's gauge wrapper. The vendored hyper adds a provider slot to every connection (review
/// F5): the future is the same type with or without a provider, so its size is the same; the
/// crates.io build without the patch is measured by `scripts/hyper-future-size.sh`.
fn stock_hyper_connection_future_size() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _first = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (plain, _) = listener.accept().await.expect("accept");
        let _second = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (wrapped, _) = listener.accept().await.expect("accept");
        let gauge = serve::ConnGauge::new();
        let stock = support_hyper::builder(&Setup::stock(Service::Static4k, true));
        let plain_size = serve::future_size(&stock.serve_connection(
            TokioIo::new(plain),
            service_fn(|_request| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    bytes::Bytes,
                >::new()))
            }),
        ));
        let gauged_size = serve::future_size(&stock.serve_connection(
            TokioIo::new(GaugedIo::new(wrapped, &gauge)),
            service_fn(|_request| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    bytes::Bytes,
                >::new()))
            }),
        ));
        let _third = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (lent_stream, _) = listener.accept().await.expect("accept");
        let lent = support_hyper::builder(&Setup::lent(Service::Static4k, true));
        let lent_size = serve::future_size(&lent.serve_connection(
            TokioIo::new(lent_stream),
            service_fn(|_request| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    bytes::Bytes,
                >::new()))
            }),
        ));
        eprintln!(
            "SERVE-HARNESS-RESULT future_size: connection future {plain_size} B (plain stream), \
             {gauged_size} B (gauged stream), {lent_size} B (plain stream, provider set)"
        );
        assert_eq!(lent_size, plain_size, "the provider is a runtime option");
        // G6 item 4: connection future <= 4 KiB
        assert!(plain_size <= 4096, "{plain_size}");
    });
}

fn stock_hyper_services_return_bodies() {
    for service in [Service::Static4k, Service::JsonEcho] {
        for timer in [false, true] {
            support_hyper::assert_service_response(&Setup::stock(service, timer));
        }
    }
}
