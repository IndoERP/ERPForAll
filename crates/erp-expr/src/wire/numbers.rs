//! JSON numbers carry value semantics in both engines: 0, 0.0 and 0e0 are the same index.
use serde::{
    Deserialize, Deserializer,
    de::{self, Visitor},
};
use std::fmt;

struct Integer(u32);
impl<'de> Deserialize<'de> for Integer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IntegerVisitor;
        impl Visitor<'_> for IntegerVisitor {
            type Value = Integer;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer in 0..=4294967295")
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Integer, E> {
                u32::try_from(value).map(Integer).map_err(E::custom)
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Integer, E> {
                u32::try_from(value).map(Integer).map_err(E::custom)
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Integer, E> {
                if value.is_finite()
                    && (0.0..=f64::from(u32::MAX)).contains(&value)
                    && value.fract() == 0.0
                {
                    // Range and integrality were checked before this conversion.
                    Ok(Integer(value as u32))
                } else {
                    Err(E::custom("expected an integer in 0..=4294967295"))
                }
            }
        }
        deserializer.deserialize_any(IntegerVisitor)
    }
}
pub(super) fn index<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    Ok(Integer::deserialize(d)?.0)
}
pub(super) fn indices<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u32>, D::Error> {
    Ok(Vec::<Integer>::deserialize(d)?
        .into_iter()
        .map(|n| n.0)
        .collect())
}
pub(super) fn optional_index<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<Integer>::deserialize(d)?.map(|n| n.0))
}
pub(super) fn pairs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(u32, u32)>, D::Error> {
    Ok(Vec::<(Integer, Integer)>::deserialize(d)?
        .into_iter()
        .map(|(a, b)| (a.0, b.0))
        .collect())
}
pub(super) fn bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    Vec::<Integer>::deserialize(d)?
        .into_iter()
        .map(|n| u8::try_from(n.0).map_err(de::Error::custom))
        .collect()
}
pub(super) fn required_option<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

// Stop materializing a hostile arena at its node limit, rather than allocating every node and
// only rejecting the length afterward. The byte/depth preflight still applies to all input.
pub(super) fn nodes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<super::WireNode>, D::Error> {
    struct RejectOverflow;
    impl<'de> de::DeserializeSeed<'de> for RejectOverflow {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, _deserializer: D) -> Result<(), D::Error> {
            // SeqAccess has established that another element exists. Do not decode its payload.
            Err(de::Error::custom("AST node array exceeds 65536 entries"))
        }
    }
    struct NodesVisitor;
    impl<'de> Visitor<'de> for NodesVisitor {
        type Value = Vec<super::WireNode>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an AST node array with at most 65536 entries")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut nodes = Vec::new();
            while nodes.len() < super::MAX_NODES {
                match seq.next_element()? {
                    Some(node) => nodes.push(node),
                    None => return Ok(nodes),
                }
            }
            // End-of-array is valid at the cap; any further element invokes the rejecting seed.
            seq.next_element_seed(RejectOverflow).map(|_| nodes)
        }
    }
    d.deserialize_seq(NodesVisitor)
}
