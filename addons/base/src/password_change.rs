//! Password wizard metadata. Secrets are request arguments, never persistent wizard state.
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

fn http_required() -> Error {
    Error::user("This method can only be accessed over HTTP")
}
fn secret_write() -> Error {
    Error::validation("Passwords must be submitted with the dedicated password operation.")
}

/// Administrator reset metadata; only selected-user rows are stored.
#[model(
    name = "change.password.wizard",
    description = "Password Reset",
    kind = "transient",
    transient_max_age_seconds = 720
)]
pub struct ChangePasswordWizard {
    /// Server-produced selected users; password values are not stored on these lines.
    #[field(one2many, comodel = "change.password.user", inverse_name = "wizard_id")]
    pub user_ids: Vec<RecId>,
}
/// One immutable selected target of an administrator reset.
#[model(name = "change.password.user", description = "Password Reset User", company_access = unrestricted(reason = "creator-owned password reset metadata; target authority is checked by Auth"), kind = "transient", transient_max_age_seconds = 720)]
pub struct ChangePasswordUser {
    /// Owning wizard.
    #[field(
        many2one,
        comodel = "change.password.wizard",
        required = true,
        index = true,
        ondelete = "cascade"
    )]
    pub wizard_id: Option<RecId>,
    /// Selected target; submission always rechecks narrow administrator authority.
    #[field(many2one, comodel = "res.users", required = true, ondelete = "cascade")]
    pub user_id: Option<RecId>,
    /// Login copied by the server when opening, never trusted as an identity.
    #[field(char, readonly = true)]
    pub user_login: Option<Arc<str>>,
    /// Always empty when read; only the dedicated submission accepts a replacement.
    #[field(
        char,
        compute = "_empty_password",
        inverse = "_refuse_password",
        copy = false,
        secret = true
    )]
    pub new_passwd: Option<Arc<str>>,
}
/// Own-password form metadata; target is always the current session user.
#[model(
    name = "change.password.own",
    description = "Change Own Password",
    kind = "transient",
    transient_max_age_seconds = 360
)]
pub struct ChangePasswordOwn {
    /// New secret, held only by the request-local operation.
    #[field(
        char,
        compute = "_empty_password",
        inverse = "_refuse_password",
        copy = false,
        secret = true
    )]
    pub new_password: Option<Arc<str>>,
    /// Exact confirmation of the submitted secret.
    #[field(
        char,
        compute = "_empty_password",
        inverse = "_refuse_password",
        copy = false,
        secret = true
    )]
    pub confirm_password: Option<Arc<str>>,
}
/// Identity form metadata; confirmation is persisted only on the authenticated session.
#[model(
    name = "res.users.identitycheck",
    description = "Confirm Your Identity",
    kind = "transient",
    transient_max_age_seconds = 360
)]
pub struct ResUsersIdentitycheck {
    /// Never stored or replayed.
    #[field(
        char,
        compute = "_empty_password",
        inverse = "_refuse_password",
        copy = false,
        secret = true
    )]
    pub password: Option<Arc<str>>,
}
/// Managed password entry declarations. Ordinary model calls have no HTTP authority.
#[extend(ResUsers)]
pub struct ResUsersPassword {
    /// Whether this saved record is the current actor, for password button visibility only.
    /// This flag never supplies authorization to a password operation.
    #[field(boolean, compute = "_compute_password_is_own")]
    pub password_is_own: bool,
}
#[methods(ResUsers)]
impl ResUsersPassword {
    #[compute(depends_context = ["uid"])]
    async fn _compute_password_is_own(
        this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
        out: &mut Assign<'_, ResUsers>,
    ) -> Result<()> {
        for (index, id) in this.as_records().ids().iter().enumerate() {
            out.set(
                index,
                ResUsersPassword::PASSWORD_IS_OWN,
                *id == this.env().uid(),
            )?;
        }
        Ok(())
    }
    #[method]
    #[rpc(effect = write)]
    async fn action_change_password_wizard(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
    ) -> Result<Value> {
        Err(http_required())
    }
    #[method]
    #[rpc(effect = write)]
    async fn preference_change_password(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
    ) -> Result<Value> {
        Err(http_required())
    }
    #[method]
    #[rpc(effect = write, model)]
    async fn change_password(
        _this: &RecordSet<ResUsers>,
        _tx: &mut Transaction,
        old_passwd: String,
        new_passwd: String,
    ) -> Result<bool> {
        drop((old_passwd, new_passwd));
        Err(http_required())
    }
}
#[methods(ChangePasswordWizard)]
impl ChangePasswordWizard {
    #[method]
    #[rpc(effect = write)]
    async fn change_password_button(
        _this: &RecordSet<ChangePasswordWizard>,
        _tx: &mut Transaction,
        changes: Vec<ValueMap>,
    ) -> Result<Value> {
        drop(changes);
        Err(http_required())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<ChangePasswordWizard>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(Error::access("Password wizard targets cannot be changed."))
    }
}
#[methods(ChangePasswordUser)]
impl ChangePasswordUser {
    /// Derive the visible login and prevent lines from being attached to another creator's wizard.
    #[method(override)]
    async fn create(
        this: &RecordSet<ChangePasswordUser>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let wizard = tx.env().records("change.password.wizard")?;
        tx.flush_model(wizard.model_id()).await?;
        tx.flush_model(created.model_id()).await?;
        let ids = created.ids();
        let rows = tx.query("SELECT w.create_uid::bigint FROM change_password_user l JOIN change_password_wizard w ON w.id = l.wizard_id WHERE l.id = ANY($1::int8[])", &[&ids]).await?;
        if rows.len() != ids.len() {
            return Err(Error::programming(
                "Password reset lines lack their owning wizard.",
            ));
        }
        for row in rows {
            let owner: i64 = row.try_get(0).map_err(|error| {
                Error::db("Invalid password wizard creator.").with_source(error)
            })?;
            if owner != this.env().uid() {
                return Err(Error::access(
                    "Password reset lines must belong to the current user's wizard.",
                ));
            }
        }
        tx.execute("UPDATE change_password_user l SET user_login = u.login FROM res_users u WHERE l.user_id = u.id AND l.id = ANY($1::int8[])", &[&ids]).await?;
        tx.invalidate_records(&created, Some(&["user_login"]))
            .await?;
        Ok(created)
    }
    #[compute(depends = [])]
    fn _empty_password(batch: &mut Batch<'_, ChangePasswordUser>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, ChangePasswordUser::NEW_PASSWD, None::<Arc<str>>)?;
        }
        Ok(())
    }
    #[method]
    async fn _refuse_password(
        _this: &RecordSet<ChangePasswordUser>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        Err(secret_write())
    }
    #[method]
    #[rpc(effect = write)]
    async fn change_password_button(
        _this: &RecordSet<ChangePasswordUser>,
        _tx: &mut Transaction,
        new_passwd: String,
    ) -> Result<Value> {
        drop(new_passwd);
        Err(http_required())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<ChangePasswordUser>,
        _tx: &mut Transaction,
        vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        if vals.contains("new_passwd") {
            return Err(secret_write());
        }
        Err(Error::access("Password wizard targets cannot be changed."))
    }
}
#[methods(ChangePasswordOwn)]
impl ChangePasswordOwn {
    #[compute(depends = [])]
    fn _empty_password(batch: &mut Batch<'_, ChangePasswordOwn>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, ChangePasswordOwn::NEW_PASSWORD, None::<Arc<str>>)?;
            batch.set(i, ChangePasswordOwn::CONFIRM_PASSWORD, None::<Arc<str>>)?;
        }
        Ok(())
    }
    #[method]
    async fn _refuse_password(
        _this: &RecordSet<ChangePasswordOwn>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        Err(secret_write())
    }
    #[method]
    #[rpc(effect = write)]
    async fn change_password(
        _this: &RecordSet<ChangePasswordOwn>,
        _tx: &mut Transaction,
        new_password: String,
        confirm_password: String,
    ) -> Result<Value> {
        drop((new_password, confirm_password));
        Err(http_required())
    }
}
#[methods(ResUsersIdentitycheck)]
impl ResUsersIdentitycheck {
    #[compute(depends = [])]
    fn _empty_password(batch: &mut Batch<'_, ResUsersIdentitycheck>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, ResUsersIdentitycheck::PASSWORD, None::<Arc<str>>)?;
        }
        Ok(())
    }
    #[method]
    async fn _refuse_password(
        _this: &RecordSet<ResUsersIdentitycheck>,
        _tx: &mut Transaction,
    ) -> Result<()> {
        Err(secret_write())
    }
    #[method]
    #[rpc(effect = write, model)]
    async fn run_check(
        _this: &RecordSet<ResUsersIdentitycheck>,
        _tx: &mut Transaction,
        password: String,
    ) -> Result<Value> {
        drop(password);
        Err(http_required())
    }
}
