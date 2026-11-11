//! Second addon of the session token plan fixtures: adds `tok_n` after
//! `test_token`'s override, through `super`.

use erp_sdk::auth::SessionTokenFields;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_token_more", depends = ["test_token"]);

/// The chained override.
#[extend(ResUsers, addon = "test_token_more")]
pub struct UsersTokenMore {}

#[methods(ResUsers, addon = "test_token_more")]
impl UsersTokenMore {
    /// `super` (the base and `test_token`'s fields), then `tok_n`.
    #[method(override)]
    async fn _get_session_token_fields(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<SessionTokenFields> {
        let fields: SessionTokenFields = sup.call(tx, this.as_records(), ()).await?;
        fields.with_field("tok_n")
    }
}
