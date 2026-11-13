//! Fixture "company escape": a model whose `company_access`
//! carries an `escape` over a stored, required boolean that is itself a stored compute (the shape
//! of `res.users.share`), one whose escape is a plain stored boolean, and one restricted through
//! a many2many of companies with an inversed field whose inverse reads the record (the create
//! path of `res.users` and its `password`). Has its own `res.company` with a hierarchy, so load
//! it without `test_rel`, `test_company` and `test_company_access`.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_company_escape");

/// `res.company` with its hierarchy.
#[model(
    name = "res.company",
    description = "Companies",
    addon = "test_company_escape",
    parent_store = true,
    parent_name = "parent_id"
)]
pub struct Company {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub parent_id: Option<RecId>,
    #[field(char, index)]
    pub parent_path: Option<Arc<str>>,
}

/// Escape over a stored compute: `share` is true when `kind` is `portal`.
#[model(
    name = "test_company_escape.doc",
    description = "Escape over a stored compute",
    addon = "test_company_escape",
    keys = [key(name = "_name_key", fields = ["name"], message = "A document name must be unique.")],
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", escape = "share")
)]
pub struct Doc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, default = "user")]
    pub kind: Option<Arc<str>>,
    #[field(boolean, compute = "_compute_share", store, required, default = false)]
    pub share: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

#[methods(Doc, addon = "test_company_escape")]
impl Doc {
    #[compute(depends = ["kind"])]
    fn _compute_share(batch: &mut Batch<'_, Doc>) -> Result<()> {
        for i in batch.indices() {
            let kind = batch.get(i, Doc::KIND)?;
            batch.set(i, Doc::SHARE, kind.as_deref() == Some("portal"))?;
        }
        Ok(())
    }
}

/// Escape over a plain stored boolean with a default.
#[model(
    name = "test_company_escape.plain",
    description = "Escape over a plain boolean",
    addon = "test_company_escape",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow", escape = "flag")
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = false, required)]
    pub flag: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// Restricted through a many2many of companies (`exact`, `deny`, as `res.users`), with a
/// non-stored field whose inverse reads the record with the caller's rights and stores the value
/// with SQL (the shape of `res.users.password`).
#[model(
    name = "test_company_escape.member",
    description = "Member restricted through its companies",
    addon = "test_company_escape",
    company_access = direct(field = "company_ids", branch = "exact", empty = "deny")
)]
pub struct Member {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "res.company",
        relation = "test_company_escape_member_company_rel",
        column1 = "member_id",
        column2 = "company_id"
    )]
    pub company_ids: Vec<RecId>,
    /// Written in clear, kept in `stored_note` by the inverse, read empty.
    #[field(char, compute = "_compute_note", inverse = "_set_note")]
    pub note: Option<Arc<str>>,
    #[field(char)]
    pub stored_note: Option<Arc<str>>,
}

#[methods(Member, addon = "test_company_escape")]
impl Member {
    #[compute(depends = [])]
    fn _compute_note(batch: &mut Batch<'_, Member>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, Member::NOTE, None::<Arc<str>>)?;
        }
        Ok(())
    }

    /// Reads each record's written value as the caller, then stores it with one statement.
    #[method]
    async fn _set_note(this: &RecordSet<Member>, tx: &mut Transaction) -> Result<()> {
        let mut ids = Vec::with_capacity(this.as_records().len());
        let mut notes = Vec::with_capacity(ids.capacity());
        for rec in this.iter() {
            let note = rec.get(tx, Member::NOTE).await?;
            notes.push(note.map(|n| n.to_string()));
            ids.push(rec.as_records().int()?);
        }
        tx.execute(
            "UPDATE test_company_escape_member m SET stored_note = v.n \
               FROM unnest($1::int8[], $2::text[]) AS v (id, n) WHERE m.id = v.id",
            &[&ids, &notes],
        )
        .await?;
        tx.invalidate_records(this.as_records(), Some(&["note", "stored_note"]))
            .await
    }
}
