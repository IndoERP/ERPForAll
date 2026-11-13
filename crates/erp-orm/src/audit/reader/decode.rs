// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{integrity, limit};
use erp_fields::Result;
use serde_json::Value;

// Parsing a flat grammar after bounding source bytes also bounds serde's intermediate tree.
// Nested values are refused by this decoder; serde's recursion limit remains enabled.
pub(super) fn targets(text: &str) -> Result<Vec<i64>> {
    if text.len() > 1024 * 1024 {
        return Err(limit());
    }
    let value: Value = serde_json::from_str(text).map_err(|_| integrity())?;
    let array = value.as_array().ok_or_else(integrity)?;
    if array.len() > 4096 {
        return Err(limit());
    }
    let mut ids = Vec::new();
    for item in array {
        let (first, last) = match item {
            Value::Number(n) => {
                let n = n.as_i64().ok_or_else(integrity)?;
                (n, n)
            }
            Value::Array(pair) if pair.len() == 2 => (
                pair[0].as_i64().ok_or_else(integrity)?,
                pair[1].as_i64().ok_or_else(integrity)?,
            ),
            _ => return Err(integrity()),
        };
        if first <= 0 || last < first {
            return Err(integrity());
        }
        let count = last
            .checked_sub(first)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(limit)?;
        if count > 4096 - ids.len() {
            return Err(limit());
        }
        ids.extend(first..=last);
    }
    Ok(ids)
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    #[test]
    fn reader_target_runs_are_exact_and_bounded() {
        let ids = [1, 2, 3, 9, 9, i64::MAX];
        let encoded = crate::audit::TargetIds::from_ids(&ids).unwrap().to_json();
        assert_eq!(targets(&encoded).unwrap(), ids);
        assert_eq!(targets("[[1,4096]]").unwrap().len(), 4096);
        for input in [
            "[0]",
            "[-1]",
            "[1.5]",
            "[[3,1]]",
            "[[1]]",
            "[[1,2,3]]",
            "[true]",
            "[[[1],2]]",
            "[9223372036854775808]",
        ] {
            let e = targets(input).unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Internal);
            assert_eq!(e.message(), "a later release: invalid stored audit data");
        }
        for input in ["[[1,4097]]", "[[1,9223372036854775807]]"] {
            let e = targets(input).unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::ResourceExhausted);
            assert_eq!(e.message(), "a later release: reader safety limit exceeded");
        }
    }
}
