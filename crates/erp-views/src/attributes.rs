//! `position="attributes"`.

use crate::apply::reject_stray_text;
use crate::arch::{Arch, NodeId};
use crate::error::{ViewError, ViewResult, py_repr};
use crate::limits::Budget;
use std::collections::HashSet;

/// Attributes holding Python expressions (plus `decoration-*`).
fn is_python_attribute(name: &str) -> bool {
    matches!(
        name,
        "readonly" | "required" | "invisible" | "column_invisible" | "t-if" | "t-elif"
    ) || name.starts_with("decoration-")
}

/// Applies the `<attribute>` children of `spec_id` to `target`. Reading and writing the target's
/// attributes, and every token and byte the edits handle, are charged to `budget`.
pub(crate) fn apply_attributes(
    source: &mut Arch,
    spec: &Arch,
    spec_id: NodeId,
    target: NodeId,
    budget: &mut Budget,
) -> ViewResult<()> {
    reject_stray_text(spec, spec_id)?;
    for child in spec.element_children(spec_id) {
        let tag = spec.tag(child);
        if tag != "attribute" {
            return Err(ViewError::UnexpectedAttributeChild(tag.to_owned()));
        }
        if let Some(grandchild) = spec.element_children(child).next() {
            return Err(ViewError::AttributeChildElement(
                spec.tag(grandchild).to_owned(),
            ));
        }
        let unknown: Vec<String> = spec
            .attrs(child)
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| {
                !matches!(*k, "name" | "add" | "remove" | "separator") && !k.starts_with("data-oe-")
            })
            .map(py_repr)
            .collect();
        if !unknown.is_empty() {
            return Err(ViewError::InvalidAttributeAttributes(unknown.join(", ")));
        }
        let name = spec
            .attr(child, "name")
            .ok_or(ViewError::AttributeWithoutName)?;
        let value = attribute_value(source, spec, child, target, name, budget)?;
        // Setting or removing scans the target's attributes once more.
        budget.charge_n(1 + source.attrs(target).len())?;
        // an empty result removes the attribute.
        if value.is_empty() {
            source.remove_attr(target, name);
        } else {
            source.set_attr(target, name, value)?;
        }
    }
    Ok(())
}

fn attribute_value(
    source: &Arch,
    spec: &Arch,
    child: NodeId,
    target: NodeId,
    name: &str,
    budget: &mut Budget,
) -> ViewResult<String> {
    let add = spec.attr(child, "add").filter(|v| !v.is_empty());
    let remove = spec.attr(child, "remove").filter(|v| !v.is_empty());
    if add.is_none() && remove.is_none() {
        return Ok(spec.text(child).to_owned());
    }
    let text = spec.text(child);
    if !text.is_empty() {
        return Err(ViewError::AttributeTextWithAddRemove(text.to_owned()));
    }
    let current = source
        .attr_charged(target, name, budget)?
        .unwrap_or_default();
    let separator = spec.attr(child, "separator");
    if is_python_attribute(name) {
        let separator = match separator {
            Some(s) => s.trim(),
            None => "and",
        };
        if separator != "and" && separator != "or" {
            return Err(ViewError::InvalidPythonSeparator {
                separator: separator.to_owned(),
                expression: name.to_owned(),
            });
        }
        edit_python(current, add, remove, separator, budget)
    } else {
        let separator = match separator {
            None => Some(","),
            Some("") => return Err(ViewError::EmptySeparator(name.to_owned())),
            // `separator=" "` squashes runs of whitespace (Python's `str.split(None)`).
            Some(" ") => None,
            Some(s) => Some(s),
        };
        edit_list(current, add, remove, separator, budget)
    }
}

fn split<'a>(value: &'a str, separator: Option<&'a str>) -> Box<dyn Iterator<Item = &'a str> + 'a> {
    match separator {
        Some(sep) => Box::new(value.split(sep).map(str::trim)),
        None => Box::new(value.split_whitespace()),
    }
}

/// List attributes (`class`, ...): drop the `remove` items, append the `add` items. One step per
/// token handled plus the bytes split; the removals are a set, so the edit is linear.
fn edit_list(
    current: &str,
    add: Option<&str>,
    remove: Option<&str>,
    separator: Option<&str>,
    budget: &mut Budget,
) -> ViewResult<String> {
    // An absent `add` or `remove` contributes no item, exactly like an empty one.
    let (add, remove) = (add.unwrap_or(""), remove.unwrap_or(""));
    budget.charge_bytes(current.len() + add.len() + remove.len())?;
    let mut to_remove = HashSet::new();
    for item in split(remove, separator) {
        budget.charge()?;
        to_remove.insert(item);
    }
    let mut joined: Vec<&str> = Vec::new();
    for item in split(current, separator) {
        budget.charge()?;
        if !item.is_empty() && !to_remove.contains(item) {
            joined.push(item);
        }
    }
    for item in split(add, separator) {
        budget.charge()?;
        if !item.is_empty() {
            joined.push(item);
        }
    }
    Ok(match separator {
        Some(sep) => joined.join(sep),
        None => joined.join(" "),
    })
}

/// The copies and the whole-operand
/// test are charged by their bytes, every pattern search by the bytes it scans.
fn edit_python(
    current: &str,
    add: Option<&str>,
    remove: Option<&str>,
    separator: &str,
    budget: &mut Budget,
) -> ViewResult<String> {
    let extra = add.map_or(0, str::len) + remove.map_or(0, str::len);
    budget.charge_bytes(current.len().saturating_add(extra))?;
    let mut value = current.to_owned();
    if let Some(remove) = remove {
        if is_whole_operand(&value, remove) {
            value.clear();
        } else {
            let patterns = [
                (format!("({remove}) {separator} "), Boundary::Before),
                (format!(" {separator} ({remove})"), Boundary::After),
                (format!("{remove} {separator} "), Boundary::Before),
                (format!(" {separator} {remove}"), Boundary::After),
            ];
            let mut found = None;
            for (pattern, boundary) in &patterns {
                if let Some(start) = find_operand(&value, pattern, *boundary, budget)? {
                    found = Some((start, pattern.len()));
                    break;
                }
            }
            if let Some((start, len)) = found {
                value.replace_range(start..start + len, "");
            }
        }
    }
    if let Some(add) = add {
        value = if value.is_empty() {
            add.to_owned()
        } else {
            format!("({value}) {separator} ({add})")
        };
    }
    Ok(value)
}

/// The `(` run that the regex skips must end exactly where `remove`'s own leading `(` run starts:
/// if `remove` has `p` leading parentheses and `value` has `opening`, the only candidate start is
/// `opening - p` (an earlier start would leave a `(` where `remove` has its first other
/// character, or extra `(` before the `)` tail when `remove` is all parentheses).
fn is_whole_operand(value: &str, remove: &str) -> bool {
    let opening = value.len() - value.trim_start_matches('(').len();
    let leading = remove.len() - remove.trim_start_matches('(').len();
    let Some(start) = opening.checked_sub(leading) else {
        return false;
    };
    value[start..]
        .strip_prefix(remove)
        .is_some_and(|rest| rest.chars().all(|c| c == ')'))
}

#[derive(Debug, Clone, Copy)]
enum Boundary {
    /// The character before the match must not continue an identifier.
    Before,
    /// The character after the match must not continue an identifier.
    After,
}

fn continues_identifier(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// First occurrence of `pattern` in `value` whose outer edge is an identifier boundary. Searches
/// resume one character after a rejected match, so multibyte text is never split. Each search
/// is charged the pattern plus the bytes it scans: overlapping rejected matches cannot make the
/// search quadratic for free.
fn find_operand(
    value: &str,
    pattern: &str,
    boundary: Boundary,
    budget: &mut Budget,
) -> ViewResult<Option<usize>> {
    let mut from = 0;
    loop {
        let Some(rest) = value.get(from..) else {
            return Ok(None);
        };
        let found = rest.find(pattern);
        let scanned = found.map_or(rest.len(), |rel| rel + pattern.len());
        budget.charge_bytes(pattern.len().saturating_add(scanned))?;
        let Some(rel) = found else {
            return Ok(None);
        };
        let start = from + rel;
        let end = start + pattern.len();
        let ok = match boundary {
            Boundary::Before => !value[..start]
                .chars()
                .next_back()
                .is_some_and(continues_identifier),
            Boundary::After => !value[end..]
                .chars()
                .next()
                .is_some_and(continues_identifier),
        };
        if ok {
            return Ok(Some(start));
        }
        from = start + value[start..].chars().next().map_or(1, char::len_utf8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::ViewLimits;

    #[test]
    fn whole_operand_handles_parentheses_literally() {
        assert!(is_whole_operand("id == 1", "id == 1"));
        assert!(is_whole_operand("((id == 1))", "id == 1"));
        assert!(is_whole_operand("((id == 1)", "id == 1"));
        assert!(is_whole_operand("((a))", "(a)"));
        assert!(!is_whole_operand("id == 1 and x", "id == 1"));
        // Literal, not a regular expression: `.` does not match any character.
        assert!(!is_whole_operand("axb", "a.b"));
    }

    fn find(value: &str, pattern: &str, boundary: Boundary) -> Option<usize> {
        let mut budget = Budget::new(&ViewLimits::default());
        find_operand(value, pattern, boundary, &mut budget).expect("within the budget")
    }

    #[test]
    fn find_operand_respects_boundaries_and_utf8() {
        assert_eq!(
            find("xid == 1 and y", "id == 1 and ", Boundary::Before),
            None
        );
        assert_eq!(
            find("a and id == 10", " and id == 1", Boundary::After),
            None
        );
        assert_eq!(find("xé and é", "é and ", Boundary::Before), None);
        assert_eq!(find("é and é", "é and ", Boundary::Before), Some(0));
    }

    /// The one-pass whole-operand test agrees with trying every start inside the `(` run.
    #[test]
    fn whole_operand_matches_trying_every_start() {
        fn every_start(value: &str, remove: &str) -> bool {
            let opening = value.len() - value.trim_start_matches('(').len();
            (0..=opening).any(|k| {
                value[k..]
                    .strip_prefix(remove)
                    .is_some_and(|rest| rest.chars().all(|c| c == ')'))
            })
        }
        let parts = [
            "", "(", "((", ")", "))", "a", "(a", "a)", "()", "(()", "a b",
        ];
        for v1 in parts {
            for v2 in parts {
                for r1 in parts {
                    for r2 in parts {
                        let (value, remove) = (format!("{v1}{v2}"), format!("{r1}{r2}"));
                        assert_eq!(
                            is_whole_operand(&value, &remove),
                            every_start(&value, &remove),
                            "value {value:?}, remove {remove:?}"
                        );
                    }
                }
            }
        }
    }
}
