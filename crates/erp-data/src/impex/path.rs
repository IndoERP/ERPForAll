use erp_fields::{Error, Result};

/// Deepest column path accepted (`a/b/c/…`).
/// Real imports use at most four levels.
pub(crate) const MAX_PATH_DEPTH: usize = 16;

/// [`split_path`], refusing paths deeper than [`MAX_PATH_DEPTH`].
pub(crate) fn checked_path(column: &str) -> Result<Vec<String>> {
    let path = split_path(column);
    if path.len() > MAX_PATH_DEPTH {
        return Err(Error::programming(format!(
            "column '{column}' is nested {} levels deep; at most {MAX_PATH_DEPTH} are accepted",
            path.len()
        )));
    }
    Ok(path)
}

/// Split a column name into its path: `x.id` and `x:id` (after any character but `/`) become
/// `x/.id` and `x/id`, then the name is split on `/`.
pub(crate) fn split_path(column: &str) -> Vec<String> {
    let dot = rewrite(column, '.', "/.id");
    let colon = rewrite(&dot, ':', "/id");
    colon.split('/').map(str::to_owned).collect()
}

/// Replace `<c><sep>id` by `<c><replacement>` for every `c` other than `/`.
fn rewrite(text: &str, sep: char, replacement: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let matches = c != '/'
            && chars.get(i + 1) == Some(&sep)
            && chars.get(i + 2) == Some(&'i')
            && chars.get(i + 3) == Some(&'d');
        out.push(c);
        if matches {
            out.push_str(replacement);
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_PATH_DEPTH, checked_path, split_path};

    fn p(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn id_suffixes_become_subpaths() {
        assert_eq!(split_path("value"), p(&["value"]));
        assert_eq!(split_path("value.id"), p(&["value", ".id"]));
        assert_eq!(split_path("value:id"), p(&["value", "id"]));
        assert_eq!(split_path("value/id"), p(&["value", "id"]));
        assert_eq!(split_path("value/.id"), p(&["value", ".id"]));
        assert_eq!(split_path(".id"), p(&[".id"]));
        assert_eq!(split_path("id"), p(&["id"]));
        assert_eq!(split_path("line_ids/m2o:id"), p(&["line_ids", "m2o", "id"]));
        // non-overlapping, like re.sub
        assert_eq!(split_path("a.id.id"), p(&["a", ".id.id"]));
        // non-ASCII before the suffix
        assert_eq!(split_path("é.id"), p(&["é", ".id"]));
    }

    #[test]
    fn deep_paths_are_refused() {
        let ok = vec!["a"; MAX_PATH_DEPTH].join("/");
        assert_eq!(checked_path(&ok).unwrap().len(), MAX_PATH_DEPTH);
        let deep = vec!["a"; MAX_PATH_DEPTH + 1].join("/");
        let err = checked_path(&deep).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("column '{deep}' is nested 17 levels deep; at most 16 are accepted")
        );
    }
}
