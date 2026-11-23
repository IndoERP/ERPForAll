// SPDX-License-Identifier: MIT OR Apache-2.0
//! The caller's model rights on a described view root.
//!
//! A view says what it offers (`create`, `edit`, `delete` on its root); the caller's access rows say
//! what the caller may do. `get_views` answers the narrower of the two, so a client shows New,
//! Duplicate, Archive and Delete only to a caller whose model rights allow them, instead of offering
//! a command the server then refuses. Only the model-level check is used here: a command the model
//! rights allow may still be refused for one record by its record rules, and that refusal stays the
//! server's answer when the command runs.

use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::views::wire;

/// The caller's model-level `create`, `write` and `unlink` rights on one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelRights {
    pub(crate) create: bool,
    pub(crate) write: bool,
    pub(crate) unlink: bool,
}

impl ModelRights {
    /// The rights of `records`' caller on their model; `records` is the empty set of that model, so
    /// only the model-level check runs.
    ///
    /// # Errors
    ///
    /// Failures that are not refusals (a policy error, a refused company context).
    pub(crate) async fn of(records: &Records, tx: &mut Transaction) -> Result<Self> {
        Ok(Self {
            create: records.has_access(tx, Operation::Create).await?,
            write: records.has_access(tx, Operation::Write).await?,
            unlink: records.has_access(tx, Operation::Unlink).await?,
        })
    }
}

fn deny(flag: &mut Option<bool>, allowed: bool) {
    if !allowed {
        *flag = Some(false);
    }
}

/// Narrow the view root's commands to `rights`. A denied right turns the root's flag off; an allowed
/// right leaves the view's own choice (`create="0"` stays off). A form without the write right
/// also loses `edit_mode`: it is read-only, so there is no Edit command to open it with. Service
/// forms and the other view types carry no record commands and are left as they are.
///
/// # Errors
///
/// The root index outside the node arena (a lowering invariant).
pub(crate) fn restrict_root(view: &mut wire::View, rights: ModelRights) -> Result<()> {
    let root = view
        .nodes
        .get_mut(view.root as usize)
        .ok_or_else(|| Error::invariant("View root is outside its node arena."))?;
    match root {
        wire::ViewNode::List {
            create,
            edit,
            delete,
            ..
        } => {
            deny(create, rights.create);
            deny(edit, rights.write);
            deny(delete, rights.unlink);
        }
        wire::ViewNode::Form {
            record_mode: wire::RecordMode::Record,
            create,
            edit,
            delete,
            edit_mode,
            ..
        } => {
            deny(create, rights.create);
            deny(edit, rights.write);
            deny(delete, rights.unlink);
            if !rights.write {
                *edit_mode = None;
            }
        }
        wire::ViewNode::Kanban {
            create,
            edit,
            delete,
            quick_create,
            records_draggable,
            archivable,
            ..
        } => {
            deny(create, rights.create);
            deny(edit, rights.write);
            deny(delete, rights.unlink);
            *quick_create &= rights.create;
            *records_draggable &= rights.write;
            *archivable &= rights.write;
        }
        wire::ViewNode::Calendar {
            create,
            edit,
            delete,
            quick_create,
            ..
        } => {
            *create &= rights.create;
            *quick_create &= rights.create;
            *edit &= rights.write;
            *delete &= rights.unlink;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModelRights = ModelRights {
        create: false,
        write: false,
        unlink: false,
    };
    const ALL: ModelRights = ModelRights {
        create: true,
        write: true,
        unlink: true,
    };

    fn view(root: serde_json::Value) -> wire::View {
        serde_json::from_value(serde_json::json!({
            "identity": {"kind": "generated"},
            "type": "form",
            "schema_major": 1,
            "renderer": "web.form",
            "root": 0,
            "nodes": [root],
        }))
        .expect("fixture view")
    }

    fn root(view: &wire::View) -> serde_json::Value {
        serde_json::to_value(&view.nodes[0]).expect("serializable root")
    }

    #[test]
    fn denied_rights_turn_the_record_form_commands_off_and_drop_read_mode() {
        let mut form = view(serde_json::json!({
            "kind": "form", "record_mode": "record", "children": [], "edit_mode": "read"
        }));
        restrict_root(&mut form, NONE).unwrap();
        assert_eq!(
            root(&form),
            serde_json::json!({"kind": "form", "record_mode": "record", "children": [],
                "create": false, "edit": false, "delete": false})
        );
    }

    #[test]
    fn granted_rights_keep_the_view_own_choice() {
        let mut list = view(serde_json::json!({"kind": "list", "children": [], "create": false}));
        restrict_root(&mut list, ALL).unwrap();
        assert_eq!(
            root(&list),
            serde_json::json!({"kind": "list", "children": [], "create": false})
        );
        let mut form = view(serde_json::json!({
            "kind": "form", "record_mode": "record", "children": [], "edit_mode": "read"
        }));
        restrict_root(&mut form, ALL).unwrap();
        assert_eq!(root(&form)["edit_mode"], "read");
        assert!(root(&form).get("create").is_none());
    }

    #[test]
    fn each_right_narrows_only_its_own_command() {
        let mut list = view(serde_json::json!({"kind": "list", "children": []}));
        restrict_root(
            &mut list,
            ModelRights {
                create: false,
                ..ALL
            },
        )
        .unwrap();
        assert_eq!(
            root(&list),
            serde_json::json!({"kind": "list", "children": [], "create": false})
        );
        let mut list = view(serde_json::json!({"kind": "list", "children": []}));
        restrict_root(
            &mut list,
            ModelRights {
                unlink: false,
                ..ALL
            },
        )
        .unwrap();
        assert_eq!(
            root(&list),
            serde_json::json!({"kind": "list", "children": [], "delete": false})
        );
    }

    #[test]
    fn a_kanban_without_rights_loses_quick_create_dragging_and_archiving() {
        let mut kanban = view(serde_json::json!({
            "kind": "kanban", "children": [], "quick_create": true, "records_draggable": true,
            "groups_draggable": true, "archivable": true, "can_open": true
        }));
        restrict_root(&mut kanban, NONE).unwrap();
        let root = root(&kanban);
        assert_eq!(root["quick_create"], false);
        assert_eq!(root["records_draggable"], false);
        assert_eq!(root["archivable"], false);
        assert_eq!(root["groups_draggable"], true);
        assert_eq!(root["can_open"], true);
        assert_eq!(root["create"], false);
        assert_eq!(root["edit"], false);
        assert_eq!(root["delete"], false);
    }

    #[test]
    fn a_service_form_is_left_as_it_is() {
        let mut service = view(serde_json::json!({
            "kind": "form", "record_mode": "service", "children": []
        }));
        restrict_root(&mut service, NONE).unwrap();
        assert_eq!(
            root(&service),
            serde_json::json!({"kind": "form", "record_mode": "service", "children": []})
        );
    }
}
