#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "safety assertions"
)]
use serde::{Serialize, Serializer, ser::SerializeSeq};
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, fmt, mem::size_of};

struct Unknown(usize);
impl Serialize for Unknown {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(None)?;
        for i in 0..self.0 {
            seq.serialize_element(&i)?;
        }
        seq.end()
    }
}
fn run<T: Serialize + ?Sized>(value: &T) -> Value {
    crate::h18_transport::materialize(value, &mut |_| Ok(())).unwrap()
}
#[test]
fn known_and_unknown_scalar_arrays_admit_actual_capacity() {
    for n in 0..=17 {
        let input: Vec<_> = (0..n).collect();
        let mut charged = 0;
        let value = crate::h18_transport::materialize(&input, &mut |bytes| {
            charged += bytes;
            Ok(())
        })
        .unwrap();
        let Value::Array(ref array) = value else {
            panic!("array")
        };
        assert!(charged >= array.capacity() * size_of::<Value>());
        assert_eq!(value, serde_json::to_value(&input).unwrap());
        assert_eq!(run(&Unknown(n)), value);
    }
}
#[test]
fn lowered_limit_refuses_unknown_array_replacement_and_is_sticky() {
    struct Swallow;
    impl Serialize for Swallow {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(None)?;
            seq.serialize_element(&0_u8)?;
            // Deliberately malicious recovery: serializer refusal must remain sticky.
            if seq.serialize_element(&1_u8).is_err() {
                return seq.end();
            }
            seq.end()
        }
    }
    let mut calls = Vec::new();
    let error = crate::h18_transport::materialize(&Swallow, &mut |bytes| {
        calls.push(bytes);
        if calls.len() > 1 {
            Err(erp_sdk::Error::validation("fixture transport limit"))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "fixture transport limit");
    assert_eq!(calls, [size_of::<Value>(), 2 * size_of::<Value>()]);
}
#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum Choice {
    Unit,
    Newtype(u32),
    Tuple(bool, String),
    Struct { text: String },
}
struct Bytes;
impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&[0, 127, 255])
    }
}
struct Collected;
impl fmt::Display for Collected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a\\\"")?;
        f.write_str("\n雪")
    }
}
impl Serialize for Collected {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}
#[test]
fn all_shapes_match_json_wire_values_with_escapes_and_map_keys() {
    for choice in [
        Choice::Unit,
        Choice::Newtype(7),
        Choice::Tuple(true, "雪\n\"\\".into()),
        Choice::Struct { text: "x".into() },
    ] {
        assert_eq!(run(&choice), serde_json::to_value(choice).unwrap());
    }
    assert_eq!(run(&Bytes), json!([0, 127, 255]));
    assert_eq!(run(&Collected), serde_json::to_value(Collected).unwrap());
    assert_eq!(
        run(&BTreeMap::from([(true, vec![Some('雪'), None])])),
        json!({"true":["雪",null]})
    );
    assert_eq!(
        run(&BTreeMap::from([(12_u128, (1, 2))])),
        json!({"12":[1,2]})
    );
    assert_eq!(run(&BTreeMap::from([(Choice::Unit, 0)])), json!({"Unit":0}));
}
#[test]
fn strings_and_map_entries_refuse_lowered_limits() {
    let error = crate::h18_transport::materialize("雪\n", &mut |_| {
        Err(erp_sdk::Error::validation("fixture text limit"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "fixture text limit");
    let error = crate::h18_transport::materialize(&json!({"a":{"b":[1,2,3]}}), &mut |_| {
        Err(erp_sdk::Error::validation("fixture map limit"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "fixture map limit");
}
#[test]
fn nonfinite_and_invalid_map_keys_are_explicit_errors() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = crate::h18_transport::materialize(&value, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(error.message(), "H18 transport rejects non-finite numbers");
    }
    let error =
        crate::h18_transport::materialize(&BTreeMap::from([(vec![1_u8], 2)]), &mut |_| Ok(()))
            .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "H18 transport map key must be a string or JSON scalar"
    );
}
#[test]
fn nesting_and_size_overflow_refuse_without_allocation_attempt() {
    struct Deep(usize);
    impl Serialize for Deep {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if self.0 == 0 {
                s.serialize_unit()
            } else {
                let mut seq = s.serialize_seq(Some(1))?;
                seq.serialize_element(&Deep(self.0 - 1))?;
                seq.end()
            }
        }
    }
    let error = crate::h18_transport::materialize(&Deep(129), &mut |_| Ok(())).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "H18 transport nesting exceeds 128 levels");
    struct Overflow;
    impl Serialize for Overflow {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_seq(Some(usize::MAX))?.end()
        }
    }
    let error = crate::h18_transport::materialize(&Overflow, &mut |_| Ok(())).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "H18 transport storage size overflow");
}
#[test]
fn finite_float_and_key_decimals_match_actual_json_wire() {
    for value in [
        0.1_f32,
        f32::MIN_POSITIVE,
        f32::MAX,
        -0.0,
        f32::from_bits(1),
    ] {
        let expected: Value = serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(run(&value), expected);
    }
    struct FloatKey;
    impl Serialize for FloatKey {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let mut map = s.serialize_map(Some(1))?;
            map.serialize_entry(&1.0_f32, &7)?;
            map.end()
        }
    }
    assert_eq!(run(&FloatKey), json!({"1.0":7}));
    #[derive(Serialize)]
    struct Tuple(u8, char);
    #[derive(Serialize)]
    struct Newtype(Vec<u8>);
    #[derive(Serialize)]
    struct Fields {
        value: Option<String>,
        text: String,
    }
    assert_eq!(
        run(&Tuple(2, '雪')),
        serde_json::to_value(Tuple(2, '雪')).unwrap()
    );
    assert_eq!(run(&Newtype(vec![1, 2])), json!([1, 2]));
    let fields = Fields {
        value: None,
        text: "\n\"\\雪".into(),
    };
    assert_eq!(run(&fields), serde_json::to_value(&fields).unwrap());
    assert_eq!(
        std::mem::size_of::<serde_json::Map<String, Value>>(),
        std::mem::size_of::<BTreeMap<String, Value>>(),
        "Re-audit if serde_json's map backend changes"
    );
    let mut map = serde_json::Map::new();
    map.insert("b".into(), json!(0));
    map.insert("a".into(), json!(0));
    assert_eq!(
        map.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "b"],
        "BTree ordering is part of the audited backend"
    );
}
#[test]
fn malformed_map_protocol_and_unrepresentable_integers_fail_loudly() {
    struct NoKey;
    impl Serialize for NoKey {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let mut map = s.serialize_map(None)?;
            map.serialize_value(&1)?;
            map.end()
        }
    }
    let error = crate::h18_transport::materialize(&NoKey, &mut |_| Ok(())).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "H18 transport map value has no key");
    let error = crate::h18_transport::materialize(&u128::MAX, &mut |_| Ok(())).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "H18 transport integer is outside JSON Value range"
    );
}
#[test]
fn stateful_collect_str_formats_once_and_refuses_before_string_growth() {
    struct Stateful<'a>(&'a Cell<usize>);
    impl fmt::Display for Stateful<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.set(self.0.get() + 1);
            if self.0.get() == 1 {
                f.write_str("first")
            } else {
                f.write_str("changed")
            }
        }
    }
    impl Serialize for Stateful<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_str(self)
        }
    }
    let calls = Cell::new(0);
    assert_eq!(run(&Stateful(&calls)), json!("first"));
    assert_eq!(calls.get(), 1);
    struct Chunks;
    impl fmt::Display for Chunks {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("ab")?;
            f.write_str("cdef")
        }
    }
    impl Serialize for Chunks {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_str(self)
        }
    }
    let mut charges = Vec::new();
    let error = crate::h18_transport::materialize(&Chunks, &mut |bytes| {
        charges.push(bytes);
        if charges.len() > 1 {
            Err(erp_sdk::Error::validation("fixture text growth limit"))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(error.message(), "fixture text growth limit");
    assert_eq!(charges, [2, 6]);
}
#[test]
fn caller_value_is_serialized_once() {
    struct Once<'a>(&'a Cell<usize>);
    impl Serialize for Once<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.set(self.0.get() + 1);
            s.serialize_bool(true)
        }
    }
    let calls = Cell::new(0);
    assert_eq!(run(&Once(&calls)), json!(true));
    assert_eq!(calls.get(), 1);
}
#[test]
fn map_layout_assumptions_require_pinned_toolchain_reaudit() {
    assert!(
        include_str!("../../../rust-toolchain.toml")
            .lines()
            .any(|line| line == "channel = \"1.98.1\""),
        "BTree layout envelope must be re-audited when Rust changes"
    );
}
