//! Real descriptor exhaustion during `accept()` pauses the accept loop instead of stopping the
//! server (review M1). This binary holds a single test: lowering the process descriptor limit
//! must not reach any other test, and every integration test binary is its own process.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_server::conn::{AcceptFailure, accept_failure};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use std::fs::File;
use std::net::{TcpListener, TcpStream};

#[test]
fn accept_under_real_descriptor_exhaustion_pauses_and_recovers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // The handshakes complete into the backlog before the limit drops.
    let _first = TcpStream::connect(address).unwrap();
    let _second = TcpStream::connect(address).unwrap();

    let original = getrlimit(Resource::Nofile);
    let lowered = original.current.map_or(256, |current| current.min(256));
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(lowered),
            maximum: original.maximum,
        },
    )
    .unwrap();
    // Fill every free descriptor slot, holes included, until the kernel refuses.
    let mut fillers = Vec::new();
    let refusal = loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(error) => break error,
        }
        assert!(fillers.len() < 100_000, "descriptor limit was not enforced");
    };
    assert_eq!(
        refusal.raw_os_error(),
        Some(libc::EMFILE),
        "filler refusal: {refusal}"
    );

    let exhausted = listener
        .accept()
        .expect_err("accept must fail without a free descriptor");
    assert_eq!(
        exhausted.raw_os_error(),
        Some(libc::EMFILE),
        "accept error: {exhausted}"
    );
    assert_eq!(accept_failure(&exhausted), AcceptFailure::Pause);

    // Once a descriptor is free again, the queued connection is still there to accept.
    drop(fillers.pop());
    let accepted = listener.accept();
    drop(fillers);
    setrlimit(Resource::Nofile, original).unwrap();
    accepted.expect("accept succeeds after a descriptor is released");
}
