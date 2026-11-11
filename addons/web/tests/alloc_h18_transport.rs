//! Safety admission proofs: allocator evidence for before-growth refusal and BTree envelopes.
//! These functional bounds are retained; this is not a product performance pin.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "isolated safety harness"
)]
use erp_testkit::alloc;
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;
use std::{cell::Cell, mem::size_of};
#[path = "../src/h18_storage.rs"]
mod h18_storage;
#[path = "../src/h18_transport.rs"]
mod h18_transport;
#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;
alloc::harness!(
    unknown_sequence_denial_precedes_replacement,
    unknown_sequence_growth_holds_old_and_new_buffers,
    map_envelope_covers_actual_insert_splits,
    map_entry_denial_precedes_first_node_allocation,
    collect_str_denial_precedes_replacement,
    bulk_map_envelope_covers_from_iterator_scratch_and_pending_nodes,
    finite_float_roundtrip_has_no_owned_heap_storage
);

struct Unknown(usize);
impl Serialize for Unknown {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        for i in 0..self.0 {
            seq.serialize_element(&i)?;
        }
        seq.end()
    }
}
fn unknown_sequence_denial_precedes_replacement() {
    const MESSAGE: &str = "fixture transport limit";
    let admitted = Cell::new(0_usize);
    let (stats, result) = alloc::measure(|| {
        h18_transport::materialize(&Unknown(2), &mut |bytes| {
            if admitted.get() == 0 {
                admitted.set(bytes);
                Ok(())
            } else {
                Err(erp_sdk::Error::validation(MESSAGE))
            }
        })
    })
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), MESSAGE);
    assert_eq!(admitted.get(), size_of::<Value>());
    // Only the first Vec and the callback's bounded Error message were allocated. If the
    // second buffer was allocated before admission this exact source-derived sum would fail.
    assert_eq!(
        stats.total_bytes,
        u64::try_from(size_of::<Value>() + MESSAGE.len()).unwrap()
    );
    println!(
        "unknown-denial Value_slot={} admitted={} stats={stats:?}",
        size_of::<Value>(),
        admitted.get()
    );
}
fn unknown_sequence_growth_holds_old_and_new_buffers() {
    let (stats, value) =
        alloc::measure(|| h18_transport::materialize(&Unknown(2), &mut |_| Ok(())).unwrap())
            .unwrap();
    let Value::Array(values) = value else {
        panic!("expected array")
    };
    assert_eq!(values.len(), 2);
    assert_eq!(values.capacity(), 2);
    let slot = u64::try_from(size_of::<Value>()).unwrap();
    assert_eq!(stats.total_bytes, 3 * slot);
    assert_eq!(
        stats.peak_bytes,
        3 * slot,
        "old one-slot and new two-slot buffers must coexist"
    );
    println!("unknown-moving-growth Value_slot={slot} stats={stats:?}");
}
struct Entries(usize);
impl Serialize for Entries {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        for i in 0..self.0 {
            map.serialize_entry(&i, &i)?;
        }
        map.end()
    }
}
fn map_envelope_covers_actual_insert_splits() {
    let (noop, ()) = alloc::measure(|| ()).unwrap();
    // Every cardinality spans leaf/root/internal splits, with actual numeric key strings.
    for n in 0..=256 {
        let charged = Cell::new(0_usize);
        let (stats, value) = alloc::measure(|| {
            h18_transport::materialize(&Entries(n), &mut |bytes| {
                charged.set(charged.get().checked_add(bytes).unwrap());
                Ok(())
            })
            .unwrap()
        })
        .unwrap();
        let Value::Object(map) = value else {
            panic!("expected object")
        };
        assert_eq!(map.len(), n);
        assert!(
            stats.total_bytes <= u64::try_from(charged.get()).unwrap(),
            "n={n} actual={stats:?} charged={}",
            charged.get()
        );
        assert!(
            stats.peak_bytes <= u64::try_from(charged.get()).unwrap().max(noop.peak_bytes),
            "n={n} actual={stats:?} charged={}",
            charged.get()
        );
        if matches!(n, 0 | 1 | 5 | 6 | 11 | 12 | 63 | 64 | 127 | 128 | 255 | 256) {
            println!("map n={n} charged={} stats={stats:?}", charged.get());
        }
    }
}
fn map_entry_denial_precedes_first_node_allocation() {
    const MESSAGE: &str = "fixture node limit";
    let (stats, result) = alloc::measure(|| {
        h18_transport::materialize(&Entries(1), &mut |bytes| {
            if bytes > 1 {
                Err(erp_sdk::Error::validation(MESSAGE))
            } else {
                Ok(())
            }
        })
    })
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), MESSAGE);
    // The unknown map first owns its one-byte numeric key. Node admission then refuses.
    assert_eq!(stats.total_bytes, u64::try_from(1 + MESSAGE.len()).unwrap());
    println!("map-node-denial stats={stats:?}");
}

fn collect_str_denial_precedes_replacement() {
    const MESSAGE: &str = "fixture text growth limit";
    struct Chunks;
    impl std::fmt::Display for Chunks {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("ab")?;
            f.write_str("cdef")
        }
    }
    impl Serialize for Chunks {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_str(self)
        }
    }
    let calls = Cell::new(0);
    let (stats, result) = alloc::measure(|| {
        h18_transport::materialize(&Chunks, &mut |_| {
            calls.set(calls.get() + 1);
            if calls.get() > 1 {
                Err(erp_sdk::Error::validation(MESSAGE))
            } else {
                Ok(())
            }
        })
    })
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), MESSAGE);
    assert_eq!(
        stats.total_bytes,
        u64::try_from(2 + MESSAGE.len()).unwrap(),
        "only first text buffer and bounded error may be allocated"
    );
    println!("collect-str-denial stats={stats:?}");
}

fn bulk_map_envelope_covers_from_iterator_scratch_and_pending_nodes() {
    let (noop, ()) = alloc::measure(|| ()).unwrap();
    use std::collections::BTreeMap;
    for n in (0_usize..=256).chain([511, 512, 1023, 1024, 1727, 1728]) {
        let node_bytes = h18_storage::btree_storage_bytes::<usize, usize>(n).unwrap();
        // The exact-size iterator's input Vec and documented at-most-N-item stable-sort
        // scratch are distinct buffers. Both are reserved before node construction.
        let input_bytes = n.checked_mul(size_of::<(usize, usize)>()).unwrap();
        let admitted = node_bytes
            .checked_add(input_bytes)
            .unwrap()
            .checked_add(input_bytes)
            .unwrap();
        let (stats, map) =
            alloc::measure(|| (0..n).rev().map(|i| (i, i)).collect::<BTreeMap<_, _>>()).unwrap();
        assert_eq!(map.len(), n);
        assert!(
            stats.total_bytes <= u64::try_from(admitted).unwrap(),
            "bulk n={n} actual={stats:?} admitted={admitted}"
        );
        assert!(
            stats.peak_bytes <= u64::try_from(admitted).unwrap().max(noop.peak_bytes),
            "bulk n={n} actual={stats:?} admitted={admitted}"
        );
        assert!(
            stats.end_bytes <= u64::try_from(node_bytes).unwrap(),
            "bulk retained nodes n={n} actual={stats:?} node-envelope={node_bytes}"
        );
        if matches!(
            n,
            0 | 1 | 11 | 12 | 143 | 144 | 256 | 511 | 512 | 1023 | 1024 | 1727 | 1728
        ) {
            println!(
                "bulk n={n} node-envelope={node_bytes} input-and-sort={input_bytes}+{input_bytes} stats={stats:?}"
            );
        }
    }
}

fn finite_float_roundtrip_has_no_owned_heap_storage() {
    for value in [
        0.1_f32,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::MAX,
        -0.0,
    ] {
        let (stats, result) =
            alloc::measure(|| h18_transport::materialize(&value, &mut |_| Ok(()))).unwrap();
        assert!(matches!(result.unwrap(), Value::Number(_)));
        assert_eq!(
            stats.total_bytes, 0,
            "bounded primitive formatting/parser must not allocate"
        );
        assert_eq!(stats.end_bytes, 0);
        println!("finite-f32 value={value} stats={stats:?}");
    }
    for value in [0.1_f64, 1e-45, -0.0] {
        let (stats, result) =
            alloc::measure(|| h18_transport::materialize(&value, &mut |_| Ok(()))).unwrap();
        assert!(matches!(result.unwrap(), Value::Number(_)));
        assert_eq!(stats.total_bytes, 0);
        assert_eq!(stats.end_bytes, 0);
        println!("finite-f64 value={value} stats={stats:?}");
    }
}
