//! Fixtures of the session token field plan tests (/042): two
//! addons that override `res.users._get_session_token_fields`.
//!
//! - `test_token` adds user fields `tok_a` (char) and, by scenario, others. [`set_scenario`] names
//!   what the override answers *the next time the plan is resolved* (once, when an `Auth` is
//!   built); a test that builds an `Auth` with a scenario serializes the two steps itself.
//! - `test_token_more` (depends on `test_token`) adds `tok_n` through `super`, so a world with both
//!   shows a chain of two addons.
//!
//! Scenarios of `test_token`: `ok` (adds `tok_a`), `unknown` (a name that is no field), `base_dup`
//! (`login` again), `badname` (an invalid identifier), `unstored` (`tok_calc`, not stored),
//! `relation` (`tok_rel`, a one2many), `datetime` (`tok_dt`), `float` (`tok_f`),
//! `text` (`tok_txt`), `unsized` (a char without a size), `bigsize` (a char over the size limit),
//! `hang` (never answers), `toomany` (seventeen names), `err`
//! (the hook fails), `panic`, `query` (the hook runs a query, which a pure hook may not) and `dup`
//! (`tok_a` twice).
//!
//! Fields not in any plan: `tok_other`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use erp_sdk::auth::SessionTokenFields;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_token", depends = ["base"]);

static SCENARIO: Mutex<&'static str> = Mutex::new("ok");
static CALLS: AtomicU64 = AtomicU64::new(0);

/// How many times the `test_token` override ran (the plan is resolved once per `Auth`).
pub fn calls() -> u64 {
    CALLS.load(Ordering::SeqCst)
}

/// What the `test_token` override answers from now on.
pub fn set_scenario(name: &'static str) {
    *lock(&SCENARIO) = name;
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// User fields of the fixtures.
#[extend(ResUsers, addon = "test_token")]
pub struct UsersToken {
    /// In the plan of `ok`.
    #[field(char, size = 128)]
    pub tok_a: Option<Arc<str>>,
    /// In the plan of `test_token_more`.
    #[field(integer)]
    pub tok_n: Option<i32>,
    /// In no plan.
    #[field(char)]
    pub tok_other: Option<Arc<str>>,
    /// Not stored.
    #[field(char, compute = "_compute_tok_calc")]
    pub tok_calc: Option<Arc<str>>,
    /// A datetime.
    #[field(datetime)]
    pub tok_dt: Option<DateTime>,
    /// A float.
    #[field(float)]
    pub tok_f: f64,
    /// An unbounded text.
    #[field(text)]
    pub tok_txt: Option<Arc<str>>,
    /// A char without a size.
    #[field(char)]
    pub tok_unsized: Option<Arc<str>>,
    /// A char whose size is over the token's limit.
    #[field(char, size = 20000)]
    pub tok_big: Option<Arc<str>>,
    /// A relation.
    #[field(one2many, comodel = "test_token.item", inverse_name = "user_id")]
    pub tok_rel: Vec<RecId>,
}

/// The many side of `tok_rel`.
#[model(
    name = "test_token.item",
    description = "Session token fixture item",
    addon = "test_token"
)]
pub struct Item {
    /// The owner.
    #[field(many2one, comodel = "res.users")]
    pub user_id: Option<RecId>,
    /// A label.
    #[field(char)]
    pub label: Option<Arc<str>>,
}

#[methods(ResUsers, addon = "test_token")]
impl UsersToken {
    #[compute(depends = [])]
    fn _compute_tok_calc(batch: &mut Batch<'_, ResUsers>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, UsersToken::TOK_CALC, None::<Arc<str>>)?;
        }
        Ok(())
    }

    /// `super`, then what the scenario asks for.
    #[method(override)]
    #[allow(
        clippy::panic,
        reason = "the fixture panics on purpose (scenario panic)"
    )]
    async fn _get_session_token_fields(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<SessionTokenFields> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        let scenario = *lock(&SCENARIO);
        let fields: SessionTokenFields = sup.call(tx, this.as_records(), ()).await?;
        match scenario {
            "ok" => fields.with_field("tok_a"),
            "unknown" => fields.with_field("no_such_field"),
            "base_dup" => fields.with_field("login"),
            "badname" => fields.with_field("tok_a; DROP TABLE res_users"),
            "unstored" => fields.with_field("tok_calc"),
            "relation" => fields.with_field("tok_rel"),
            "datetime" => fields.with_field("tok_dt"),
            "float" => fields.with_field("tok_f"),
            "text" => fields.with_field("tok_txt"),
            "unsized" => fields.with_field("tok_unsized"),
            "bigsize" => fields.with_field("tok_big"),
            "hang" => {
                std::future::pending::<()>().await;
                Ok(fields)
            }
            "dup" => fields.with_field("tok_a")?.with_field("tok_a"),
            "toomany" => {
                let mut fields = fields;
                for n in 0..17 {
                    fields = fields.with_field(&format!("tok_x{n}"))?;
                }
                Ok(fields)
            }
            "err" => Err(Error::validation("token hook secret detail")),
            "panic" => panic!("token hook secret panic"),
            "query" => {
                tx.query("SELECT 1", &[]).await?;
                Ok(fields)
            }
            other => Err(Error::programming(format!("unknown scenario {other}"))),
        }
    }
}
