//! Fixture for a constraint that names both a scalar field and an x2many field (
//! review finding D4): it must see the x2many values the same write or create sets, and run
//! exactly once.

use std::sync::atomic::{AtomicUsize, Ordering};

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_mixed_constraint");

/// How many times the constraint ran in this process.
pub static RUNS: AtomicUsize = AtomicUsize::new(0);

/// A group whose `flagged` boolean requires at least one line.
#[model(
    name = "test_mixc.group",
    description = "test_mixc.group",
    addon = "test_mixed_constraint"
)]
pub struct MixcGroup {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean)]
    pub flagged: bool,
    #[field(one2many, comodel = "test_mixc.line", inverse_name = "group_id")]
    pub line_ids: Vec<RecId>,
}

#[methods(MixcGroup, addon = "test_mixed_constraint")]
impl MixcGroup {
    #[constrains("flagged", "line_ids")]
    async fn _check_flagged_has_lines(
        this: &RecordSet<MixcGroup>,
        tx: &mut Transaction,
    ) -> Result<()> {
        RUNS.fetch_add(1, Ordering::SeqCst);
        let records = this.as_records();
        let flagged = records.values(tx, "flagged").await?;
        for (i, flag) in flagged.iter().enumerate() {
            if *flag != Value::Bool(true) {
                continue;
            }
            let one = records.browse_ids([records.rec_ids()[i]]);
            if one.related(tx, "line_ids").await?.is_empty() {
                return Err(Error::validation("A flagged group needs a line"));
            }
        }
        Ok(())
    }
}

/// A line of a group.
#[model(
    name = "test_mixc.line",
    description = "test_mixc.line",
    addon = "test_mixed_constraint"
)]
pub struct MixcLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_mixc.group", ondelete = "cascade")]
    pub group_id: Option<RecId>,
}
