//! Non-artifact entries excluded by release preparation's ignore_patterns inventory.
//! Kept dependency-free so the CLI build script and runtime snapshot share selection.

/// These names are packaging/tooling metadata, never lifecycle data inputs.
pub(crate) fn excluded(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | "target" | ".git" | "__pycache__" | ".DS_Store"
    )
}
