//! API key metadata. Native credentials are inserted only by the security store.
use erp_sdk::prelude::*;

/// Model-owned columns of the key store; the digest is a private SQL column.
#[model(
    name = "res.users.apikeys",
    description = "API Keys",
    company_access = unrestricted(reason = "the linked users have no generated company restriction; native key issuance controls writes"),
    indexes = [
        ("_user", "(user_id)"),
        ("_expiration", "(expiration_date,id) WHERE expiration_date IS NOT NULL"),
        ("_legacy", "(legacy_index) WHERE legacy_index IS NOT NULL")
    ]
)]
pub struct ResUsersApikeys {
    /// Human label, never the credential.
    #[field(char, size = 128, required)]
    pub name: Option<Arc<str>>,
    /// Delegating subject; deletion revokes its keys.
    #[field(many2one, comodel = "res.users", required, ondelete = "cascade")]
    pub user_id: Option<RecId>,
    /// Issuing user, retained until that user is deleted.
    #[field(many2one, comodel = "res.users", required, ondelete = "cascade")]
    pub created_by: Option<RecId>,
    /// Principal kind; agent and service admission is still refused by the store.
    #[field(char, required)]
    pub kind: Option<Arc<str>>,
    /// Route audience.
    #[field(char, required)]
    pub scope: Option<Arc<str>>,
    /// Digest key identifier, inaccessible to generic callers.
    #[field(integer, required, groups = ".")]
    pub digest_kid: i32,
    /// Eight characters for display only.
    #[field(char, size = 8, required)]
    pub prefix: Option<Arc<str>>,
    /// Sorted model allowlist; SQL NULL means unrestricted.
    #[field(json, groups = ".")]
    pub models: Option<erp_sdk::serde_json::Value>,
    /// Sorted method allowlist; SQL NULL means unrestricted.
    #[field(json, groups = ".")]
    pub methods: Option<erp_sdk::serde_json::Value>,
    /// Whether the principal is read-only.
    #[field(boolean, required)]
    pub read_only: bool,
    /// Subject capability inheritance policy.
    #[field(char, required)]
    pub capabilities: Option<Arc<str>>,
    /// Naive UTC expiration, inclusive; NULL is persistent.
    #[field(datetime)]
    pub expiration_date: Option<DateTime>,
    /// Last successful use, naive UTC like other model datetimes.
    #[field(datetime)]
    pub last_used_at: Option<DateTime>,
    /// Imported legacy lookup prefix; no producer in this slice.
    #[field(char, size = 8, groups = ".")]
    pub legacy_index: Option<Arc<str>>,
    /// Imported legacy hash, never returned by ordinary reads.
    #[field(char, groups = ".")]
    pub legacy_hash: Option<Arc<str>>,
}

#[methods(ResUsersApikeys, addon = "base")]
impl ResUsersApikeys {
    /// Generic creation cannot bypass native issuance, even under sudo.
    #[method(override)]
    async fn create(
        _this: &RecordSet<ResUsersApikeys>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(Error::access(
            "API keys must be created through a later release issuance",
        ))
    }

    /// Authority and credentials are immutable through generic writes.
    #[method(override)]
    async fn write(
        _this: &RecordSet<ResUsersApikeys>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(Error::access(
            "API key authority and credentials are immutable",
        ))
    }

    /// Generic deletion cannot bypass programmatic management's ownership gate.
    #[method(override)]
    async fn unlink(
        _this: &RecordSet<ResUsersApikeys>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(Error::access(
            "API keys must be revoked through the dedicated revoke method",
        ))
    }

    /// Generate a replacement; return its one-time secret only after RPC commits.
    #[method]
    #[rpc(effect = write, model)]
    async fn generate(
        _this: &RecordSet<ResUsersApikeys>,
        tx: &mut Transaction,
        key: String,
        scope: String,
        name: String,
        expiration_date: Option<String>,
    ) -> Result<String> {
        let expiration_date = expiration_date
            .as_deref()
            .map(|text| {
                if text.len() > 19 {
                    return Err(Error::validation(
                        "expiration_date must be a UTC date or datetime",
                    ));
                }
                DateTime::parse_odoo(text).ok_or_else(|| {
                    Error::validation("expiration_date must be a UTC date or datetime")
                })
            })
            .transpose()?;
        let key = erp_sdk::auth::Zeroizing::new(key);
        let issued =
            erp_sdk::auth::apikey_manage::generate(tx, &key, &scope, &name, expiration_date)
                .await?;
        Ok(issued.into_secret().expose().to_owned())
    }

    /// Revoke an owned active key, including the caller's current credential.
    #[method]
    #[rpc(effect = write, model)]
    async fn revoke(
        _this: &RecordSet<ResUsersApikeys>,
        tx: &mut Transaction,
        key: String,
    ) -> Result<bool> {
        let key = erp_sdk::auth::Zeroizing::new(key);
        erp_sdk::auth::apikey_manage::revoke(tx, &key).await
    }
}
