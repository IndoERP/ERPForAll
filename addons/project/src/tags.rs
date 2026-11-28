// SPDX-License-Identifier: MIT OR Apache-2.0
//! `project.tags`: a label projects and tasks can carry.

use erp_sdk::prelude::*;

/// A named, coloured label; names are unique.
#[model(
    name = "project.tags",
    description = "Project Tags",
    order = "name, id",
    constraints = [("_name_uniq", "unique(name)", "A tag with this name already exists.")]
)]
pub struct ProjectTags {
    /// Tag name.
    #[field(char, required = true, index = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Colour of the tag chip: an index into the client's palette, 0 is no colour.
    #[field(integer, default = 0, string = "Color")]
    pub color: i32,
}
