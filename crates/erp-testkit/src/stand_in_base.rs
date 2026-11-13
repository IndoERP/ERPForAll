// SPDX-License-Identifier: MIT OR Apache-2.0
//! The stand-in `base` addon directory for fixtures that link the real code of `addons/base`
//! but do not load its shipped data.
//!
//! Base's code declares things in code (today the General settings: policy group `group_system`
//! and three link actions) and its register hook refuses a declaration whose group or link target
//! does not exist. A fixture's base manifest must therefore ship exactly those records. They are
//! copied out of base's own data files at call time, so their content cannot drift; only the
//! NAMES of the records base's code needs are listed here, once ([`REQUIRED_ACTIONS`]). When base
//! starts needing another record, add its xml id here. Drift fails loudly, not silently: every
//! fixture that uses this directory installs base's real code (for example
//! `erp-test-models/tests/human_uninstall_impact_base.rs`), so the register hook refuses a
//! missing record with its own message naming it.
//!
//! Compat note: the compat step path-depends on the current `erp-testkit`, so a
//! baseline test file that calls [`write_stand_in_base`] follows base's needs without a re-point.

use std::io;
use std::path::{Path, PathBuf};

/// Link actions of base's settings declarations, as `(data file, xml id)` in `addons/base/data`.
pub const REQUIRED_ACTIONS: [(&str, &str); 3] = [
    ("settings_views.xml", "settings_users_action"),
    ("settings_views.xml", "settings_languages_action"),
    ("settings_views.xml", "settings_companies_action"),
];

/// Groups file copied whole: self-contained, it holds `group_system` and what it implies.
const GROUPS_FILE: &str = "bootstrap_groups.xml";

fn base_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/base/data")
}

fn bad(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// The `<record id="id" ...>...</record>` text of `source`, without its `view_id` line (the view
/// is not part of the stand-in).
fn record(source: &str, file: &str, id: &str) -> io::Result<String> {
    let start = source
        .find(&format!("<record id=\"{id}\""))
        .ok_or_else(|| bad(format!("stand-in base: record '{id}' not found in {file}")))?;
    let len = source[start..].find("</record>").ok_or_else(|| {
        bad(format!(
            "stand-in base: record '{id}' in {file} is not closed"
        ))
    })?;
    let text = &source[start..start + len + "</record>".len()];
    let mut kept = Vec::new();
    for line in text.lines() {
        if !line.contains("name=\"view_id\"") {
            kept.push(line);
        } else if !(line.contains("<field") && (line.contains("/>") || line.contains("</field>"))) {
            // Dropping only part of a field would leave broken XML in the stand-in.
            return Err(bad(format!(
                "stand-in base: record '{id}' in {file} has a view_id field spanning several lines; \
                 keep it on one line or teach record() to drop it"
            )));
        }
    }
    Ok(kept.join("\n"))
}

/// Write `<root>/base/` (manifest, `data/groups.xml`, `data/settings_actions.xml`).
///
/// # Errors
/// I/O errors, or a record of [`REQUIRED_ACTIONS`] missing from base's real data files.
pub fn write_stand_in_base(root: &Path) -> io::Result<()> {
    let data = root.join("base/data");
    std::fs::create_dir_all(&data)?;
    std::fs::write(
        root.join("base/manifest.toml"),
        "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\n\
         data = ['data/groups.xml', 'data/settings_actions.xml']\n",
    )?;
    std::fs::copy(base_data_dir().join(GROUPS_FILE), data.join("groups.xml"))?;
    let mut actions = String::from("<erp>\n");
    for (file, id) in REQUIRED_ACTIONS {
        let source = std::fs::read_to_string(base_data_dir().join(file))?;
        actions.push_str(&record(&source, file, id)?);
        actions.push('\n');
    }
    actions.push_str("</erp>\n");
    std::fs::write(data.join("settings_actions.xml"), actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_named_records_without_view_link() {
        let dir = std::env::temp_dir().join(format!("stand-in-base-{}", std::process::id()));
        write_stand_in_base(&dir).unwrap();
        let actions = std::fs::read_to_string(dir.join("base/data/settings_actions.xml")).unwrap();
        for (_, id) in REQUIRED_ACTIONS {
            assert!(actions.contains(&format!("id=\"{id}\"")), "{id}");
        }
        assert!(!actions.contains("view_id"));
        assert!(
            std::fs::read_to_string(dir.join("base/data/groups.xml"))
                .unwrap()
                .contains("id=\"group_system\"")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_view_id_field_spanning_lines_is_refused_by_record_name() {
        let src = "<record id=\"r\" model=\"m\">\n<field\n name=\"view_id\" ref=\"v\"/>\n</record>";
        let err = record(src, "x.xml", "r").unwrap_err().to_string();
        assert!(err.contains("'r'") && err.contains("view_id"), "{err}");
        let one = "<record id=\"r\" model=\"m\">\n<field name=\"view_id\" ref=\"v\"/>\n</record>";
        assert!(!record(one, "x.xml", "r").unwrap().contains("view_id"));
    }

    #[test]
    fn a_missing_record_is_named() {
        let err = record("<erp></erp>", "x.xml", "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("'nope'") && err.contains("x.xml"), "{err}");
    }
}
