//! Snapshot descriptor and xid8 tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_store_wire::{Limit, SnapshotDescriptor, WireError, WireLimits, Xid8};

fn x(value: u64) -> Xid8 {
    Xid8::new(value).expect("nonzero xid")
}

fn descriptor(xmin: u64, xmax: u64, xip: &[u64], own: Option<u64>) -> SnapshotDescriptor {
    SnapshotDescriptor::new(
        x(xmin),
        x(xmax),
        xip.iter().copied().map(x).collect(),
        own.map(x),
    )
    .expect("valid descriptor")
}

fn bad_descriptor(path: &str, reason: &str) -> WireError {
    WireError::BadDescriptor {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Assert the refusal's kind, code, path and full message together.
fn assert_refused(
    actual: Result<SnapshotDescriptor, WireError>,
    expected: WireError,
    message: &str,
) {
    let err = actual.expect_err("must be refused");
    assert_eq!(err, expected);
    assert_eq!(err.code(), expected.code());
    assert_eq!(err.path(), expected.path());
    assert_eq!(err.to_string(), message);
}

#[test]
fn descriptor_invariants_and_parsing() {
    // Canonical decimal strings round-trip through the number and back.
    for value in [1_u64, 9, 10, 4_294_967_296, u64::MAX] {
        let parsed = Xid8::parse(&value.to_string()).expect("canonical");
        assert_eq!(parsed.get(), value);
        assert_eq!(parsed.to_string(), value.to_string());
    }
    // Accessors return what the constructor was given.
    let d = descriptor(100, 104, &[100, 103], Some(104));
    assert_eq!(
        (d.xmin().get(), d.xmax().get(), d.own().map(Xid8::get)),
        (100, 104, Some(104))
    );
    assert_eq!(
        d.xip().iter().map(|v| v.get()).collect::<Vec<_>>(),
        vec![100, 103]
    );
    // The PostgreSQL text and the wire strings build the same descriptor as the typed constructor.
    let limits = WireLimits::default();
    assert_eq!(
        SnapshotDescriptor::parse_pg_snapshot("100:104:100,103", Some(x(104))),
        Ok(d.clone())
    );
    assert_eq!(
        SnapshotDescriptor::from_wire_strings("100", "104", &["100", "103"], Some("104"), &limits),
        Ok(d)
    );
    assert_eq!(
        SnapshotDescriptor::parse_pg_snapshot("7:7:", None),
        Ok(descriptor(7, 7, &[], None))
    );

    // Each malformed xid string is refused with its own reason and the path of the member.
    let xid_cases: [(&str, &str); 7] = [
        ("", "empty"),
        ("-5", "not a decimal digit"),
        ("+5", "not a decimal digit"),
        ("007", "leading zero"),
        ("0", "zero is not an xid8"),
        ("18446744073709551616", "above 2^64-1"),
        ("1e3", "not a decimal digit"),
    ];
    for (text, reason) in xid_cases {
        let expected = bad_descriptor("asof.xmin", reason);
        let message = format!("asof.xmin: bad descriptor: {reason}");
        assert_refused(
            SnapshotDescriptor::from_wire_strings(text, "9", &[], None, &limits),
            expected,
            &message,
        );
    }
    assert_refused(
        SnapshotDescriptor::from_wire_strings("1", "9", &["2", "x"], None, &limits),
        bad_descriptor("asof.xip[1]", "not a decimal digit"),
        "asof.xip[1]: bad descriptor: not a decimal digit",
    );
    assert_refused(
        SnapshotDescriptor::from_wire_strings("1", "9", &[], Some("01"), &limits),
        bad_descriptor("asof.own", "leading zero"),
        "asof.own: bad descriptor: leading zero",
    );
    assert_eq!(Xid8::parse("007").expect_err("refused").path(), Some("xid"));
    assert_eq!(
        Xid8::new(0).expect_err("refused"),
        bad_descriptor("xid", "zero is not an xid8")
    );

    // Invariants.
    let make = |xmin: u64, xmax: u64, xip: &[u64], own: Option<u64>| {
        SnapshotDescriptor::new(
            x(xmin),
            x(xmax),
            xip.iter().copied().map(x).collect(),
            own.map(x),
        )
    };
    assert_refused(
        make(6, 5, &[], None),
        bad_descriptor("asof", "xmin is above xmax"),
        "asof: bad descriptor: xmin is above xmax",
    );
    assert_refused(
        make(1, 9, &[5, 3], None),
        bad_descriptor("asof.xip[1]", "xip is not strictly ascending"),
        "asof.xip[1]: bad descriptor: xip is not strictly ascending",
    );
    assert_refused(
        make(1, 9, &[3, 3], None),
        bad_descriptor("asof.xip[1]", "xip is not strictly ascending"),
        "asof.xip[1]: bad descriptor: xip is not strictly ascending",
    );
    assert_refused(
        make(4, 9, &[3], None),
        bad_descriptor("asof.xip[0]", "xip entry is outside [xmin, xmax)"),
        "asof.xip[0]: bad descriptor: xip entry is outside [xmin, xmax)",
    );
    assert_refused(
        make(1, 9, &[9], None),
        bad_descriptor("asof.xip[0]", "xip entry is outside [xmin, xmax)"),
        "asof.xip[0]: bad descriptor: xip entry is outside [xmin, xmax)",
    );
    assert_refused(
        make(1, 9, &[2, 4], Some(4)),
        bad_descriptor("asof.own", "own is in xip"),
        "asof.own: bad descriptor: own is in xip",
    );
    // A1: own is null or at least xmax; a snapshot is fixed before the xid is assigned.
    assert_refused(
        make(100, 105, &[100, 103], Some(104)),
        bad_descriptor("asof.own", "own is below xmax"),
        "asof.own: bad descriptor: own is below xmax",
    );
    assert_refused(
        make(100, 105, &[], Some(101)),
        bad_descriptor("asof.own", "own is below xmax"),
        "asof.own: bad descriptor: own is below xmax",
    );
    assert!(make(100, 105, &[], Some(105)).is_ok());
    assert_refused(
        SnapshotDescriptor::from_wire_strings(
            "100",
            "105",
            &["100"],
            Some("104"),
            &WireLimits::default(),
        ),
        bad_descriptor("asof.own", "own is below xmax"),
        "asof.own: bad descriptor: own is below xmax",
    );
    assert_refused(
        SnapshotDescriptor::parse_pg_snapshot("100:105:100", Some(x(104))),
        bad_descriptor("asof.own", "own is below xmax"),
        "asof.own: bad descriptor: own is below xmax",
    );
    // PostgreSQL text with the wrong overall shape.
    for text in ["", "1:2", "1:2:3:4", "a:2:"] {
        let err = SnapshotDescriptor::parse_pg_snapshot(text, None).expect_err("refused");
        assert_eq!(err.code(), "bad_descriptor", "{text:?}");
    }
    assert_refused(
        SnapshotDescriptor::parse_pg_snapshot("1:2", None),
        bad_descriptor("snapshot", "expected xmin:xmax:xip,..."),
        "snapshot: bad descriptor: expected xmin:xmax:xip,...",
    );

    // No truncation path: a list above the limit is an error naming the limit.
    let lowered = WireLimits::default().lowered(Limit::Xip, 2).expect("lower");
    let maximum = lowered.get(Limit::Xip);
    let ok = SnapshotDescriptor::new_with_limits(x(1), x(9), vec![x(2), x(3)], None, &lowered);
    assert!(ok.is_ok());
    let too_long =
        SnapshotDescriptor::new_with_limits(x(1), x(9), vec![x(2), x(3), x(4)], None, &lowered);
    assert_refused(
        too_long,
        WireError::LimitExceeded {
            limit: "xip",
            path: "asof.xip".to_owned(),
            maximum,
        },
        "asof.xip: limit xip exceeded (maximum 2)",
    );
}

#[test]
fn reflection_truth_table() {
    let d = descriptor(100, 110, &[103, 105], Some(120));
    // below xmin, inside and not in progress, in progress, = xmax, above xmax, = own (own >= xmax)
    let table: [(&str, u64, bool); 6] = [
        ("below xmin", 99, true),
        ("inside and completed", 104, true),
        ("in progress", 103, false),
        ("equal to xmax", 110, false),
        ("above xmax", 111, false),
        ("own", 120, true),
    ];
    for (label, xid, expected) in table {
        assert_eq!(d.reflects(x(xid)), expected, "{label}");
    }
    // Own at xmax is reflected (A1: own is never below xmax).
    assert!(descriptor(100, 110, &[103], Some(110)).reflects(x(110)));

    // Independent oracle: the known set K(D) built from its definition over a window of xids.
    let cases = [
        descriptor(5, 12, &[5, 7, 11], None),
        descriptor(5, 12, &[], Some(13)),
        descriptor(5, 5, &[], Some(5)),
        descriptor(1, 20, &[2, 19], Some(20)),
    ];
    for d in &cases {
        for value in 1..=25_u64 {
            let (xmin, xmax) = (d.xmin().get(), d.xmax().get());
            let in_xip = d.xip().iter().any(|v| v.get() == value);
            let known = value < xmin
                || (value >= xmin && value < xmax && !in_xip)
                || d.own().map(Xid8::get) == Some(value);
            assert_eq!(d.reflects(x(value)), known, "{d:?} xid {value}");
        }
    }

    // Completed count n(D) = xmax - |xip|, the order key between two answers.
    assert_eq!(
        descriptor(100, 104, &[100, 103], Some(104)).completed_count(),
        102
    );
    assert_eq!(descriptor(7, 7, &[], None).completed_count(), 7);
    assert_eq!(
        descriptor(1, u64::MAX, &[], None).completed_count(),
        u64::MAX
    );
}
