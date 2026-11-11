use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_sudo_commands");

/// `test_orm.user`: the owner of the many2many field.
#[model(
    name = "test_sudo.user",
    description = "test_sudo.user",
    addon = "test_sudo_commands",
    allow_sudo_commands = false
)]
pub struct SudoUser {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "test_sudo.group")]
    pub group_ids: Vec<RecId>,
}

/// `test_orm.group`: protected comodel of the many2many.
#[model(
    name = "test_sudo.group",
    description = "test_sudo.group",
    addon = "test_sudo_commands",
    allow_sudo_commands = false
)]
pub struct SudoGroup {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2many, comodel = "test_sudo.user")]
    pub user_ids: Vec<RecId>,
}

/// A one2many owner (`res.partner` stand-in) whose lines are protected.
#[model(
    name = "test_sudo.owner",
    description = "test_sudo.owner",
    addon = "test_sudo_commands"
)]
pub struct SudoOwner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_sudo.line", inverse_name = "owner_id")]
    pub line_ids: Vec<RecId>,
    #[field(one2many, comodel = "test_sudo.free", inverse_name = "owner_id")]
    pub free_ids: Vec<RecId>,
}

/// A protected line (`res.users` stand-in).
#[model(
    name = "test_sudo.line",
    description = "test_sudo.line",
    addon = "test_sudo_commands",
    allow_sudo_commands = false
)]
pub struct SudoLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub rank: i32,
    #[field(many2one, comodel = "test_sudo.owner", ondelete = "set null")]
    pub owner_id: Option<RecId>,
}

/// A line whose model keeps the default `_allow_sudo_commands = True`.
#[model(
    name = "test_sudo.free",
    description = "test_sudo.free",
    addon = "test_sudo_commands"
)]
pub struct SudoFree {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_sudo.owner", ondelete = "set null")]
    pub owner_id: Option<RecId>,
}
