// SPDX-License-Identifier: MIT OR Apache-2.0

use erp_sdk::prelude::*;

/// External ids of the master tags the chart definition relies on; deleting one is refused.
/// Amendment A3 (`protected` records) replaces this
/// list once the data loader supports it.
const MASTER_TAGS: [&str; 3] = [
    "account_tag_operating",
    "account_tag_financing",
    "account_tag_investing",
];

/// `account.account.tag`: a label applied to accounts (cash-flow classes, custom reports) or to
/// taxes (tax grids). Applicability is a classification, not a constraint.
#[model(
    name = "account.account.tag",
    description = "Account Tag",
    order = "name, id",
    keys = [key(
        name = "_name_uniq",
        fields = ["name", "applicability", "country_id"],
        message = "This country already has a tag of this name for the same use."
    )],
)]
pub struct AccountAccountTag {
    /// Tag name.
    #[field(char, required = true, string = "Tag Name")]
    pub name: Option<Arc<str>>,
    /// Where the tag is used.
    #[field(
        selection,
        selection = [("accounts", "Accounts"), ("taxes", "Taxes"), ("products", "Products")],
        required = true,
        default = "accounts"
    )]
    pub applicability: Option<Arc<str>>,
    /// Color index of the tag.
    #[field(integer, string = "Color Index")]
    pub color: i32,
    /// Inactive tags are hidden without being removed.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Country for which the tag is available, when applied on taxes.
    #[field(many2one, comodel = "res.country", string = "Country")]
    pub country_id: Option<RecId>,
}

#[methods(AccountAccountTag, addon = "account")]
impl AccountAccountTag {
    /// A master tag cannot be deleted while the addon that defines it is installed.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountAccountTag>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.is_empty() && this.env().registry().model("ir.model.data").is_some() {
            let ids = this.ids();
            let names: Vec<String> = MASTER_TAGS.iter().map(|n| (*n).to_owned()).collect();
            let rows = tx
                .query(
                    "SELECT res_id FROM ir_model_data WHERE model = 'account.account.tag' \
                     AND module = 'account' AND name = ANY($2::text[]) AND res_id = ANY($1::int8[]) \
                     LIMIT 1",
                    &[&ids, &names],
                )
                .await?;
            if let Some(row) = rows.first() {
                let id: i64 = row
                    .try_get(0)
                    .map_err(|e| Error::db(format!("Master tag id could not be read: {e}")))?;
                let name = this
                    .browse([id])?
                    .mapped(tx, AccountAccountTag::NAME)
                    .await?;
                let name = name.first().and_then(|n| n.as_deref()).unwrap_or_default();
                return Err(Error::validation(format!(
                    "Tag {name} is part of the chart of accounts the accounting app installs, so it cannot be deleted."
                )));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}
