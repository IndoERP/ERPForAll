//! Sample addon `sample_library_wasm`: Rust code that runs as
//! a sandboxed WebAssembly component and extends the data-only `sample_library` of this repository.
//!
//! - **Lend** on an available book opens a wizard (`sample_library.lend.wizard`, a transient model
//!   of this package) with the borrower and a due date [`LOAN_DAYS`] days from today; confirming it
//!   creates the loan (`out`, loan date today), marks the book unavailable, counts the lending,
//!   remembers the borrower and shows a notification;
//! - **Return** on the book or on the loan sets the loan `returned` with today's return date and
//!   makes the book available again;
//! - **Renew** on a loan that is out moves its due date [`LOAN_DAYS`] days further and counts the
//!   renewal; a returned loan is refused, and the policy package limits how often;
//! - `lending_summary` (not stored) and `popular` (stored) of a book are computed from the
//!   counters; a constraint refuses a loan renewed more than [`MAX_RENEWALS`] times;
//! - overrides of `create` (a book's title is trimmed), `write` (a hand-edited due date leaves a
//!   note) and `unlink` (a renewed loan is kept) each call `super()`, the core implementation with
//!   the caller's rights.
//!
//! The book and loan models belong to `sample_library` (its `models/*.toml`): `model_ref!` names
//! them and the fields this addon reads and writes; only the fields of `#[extend]` are this
//! addon's own. The component is built with `erp addon build sample_library_wasm --target wasm`,
//! packed with `erp addon package` and installed with `erp addon install`; see
//! .

use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};

erp_sdk::addon!(name = "sample_library_wasm", depends = ["sample_library"]);

// The leaf crate of a component exports it; natively this expands to nothing. Another addon that
// builds on this crate (`sample_library_wasm_policy`) is the leaf of its own component and depends
// on this crate without the `component` feature, so the component is exported once.
#[cfg(feature = "component")]
erp_sdk::export_component!();

erp_sdk::model_ref!(
    /// `sample_library.book`, declared by the data-only addon `sample_library`.
    pub Book = "sample_library.book" {
        title: Option<Arc<str>>,
        available: bool,
        loan_ids: X2many<Loan>,
    }
);
erp_sdk::model_ref!(
    /// `sample_library.loan`, declared by the data-only addon `sample_library`.
    pub Loan = "sample_library.loan" {
        book_id: Many2one<Book>,
        borrower_id: Many2one<Partner>,
        loan_date: Option<Date>,
        due_date: Option<Date>,
        return_date: Option<Date>,
        state: Option<Arc<str>>,
    }
);
erp_sdk::model_ref!(
    /// `res.partner`, declared by `base`: the borrower of a loan.
    pub Partner = "res.partner" {
        name: Option<Arc<str>>,
    }
);

/// A loan runs this many days, and a renewal adds as many.
pub const LOAN_DAYS: i32 = 14;

/// A book lent this many times counts as popular.
pub const POPULAR_AFTER: i32 = 3;

/// A loan may be renewed this many times.
pub const MAX_RENEWALS: i32 = 5;

/// The loan state of a book that is out.
const OUT: &str = "out";

/// This addon's fields on a book.
#[extend(Book)]
pub struct BookStats {
    /// How many times the book was lent.
    #[field(integer, string = "Times lent")]
    pub times_lent: i32,
    /// Who borrowed it last.
    #[field(char, string = "Last borrower")]
    pub last_borrower: Option<Arc<str>>,
    /// A sentence about the book's lending history (computed, not stored).
    #[field(char, string = "Lending summary", compute = "compute_lending_summary")]
    pub lending_summary: Option<Arc<str>>,
    /// Whether the book was lent at least [`POPULAR_AFTER`] times (computed and stored).
    #[field(boolean, string = "Popular", compute = "compute_popular", store = true)]
    pub popular: bool,
}

/// This addon's fields on a loan.
#[extend(Loan)]
pub struct LoanRenewals {
    /// How many times the loan was renewed.
    #[field(integer, string = "Renewals")]
    pub renewals: i32,
    /// The note of the last renewal.
    #[field(char, string = "Renewal note")]
    pub renewal_note: Option<Arc<str>>,
}

/// The Lend wizard: who borrows the book, and until when.
#[model(
    name = "sample_library.lend.wizard",
    description = "Lend a Book",
    kind = "transient",
    company_access = unrestricted(reason = "The lend wizard is temporary; existing model access and loan rules govern its records")
)]
pub struct LendWizard {
    /// The book to lend: set by the Lend button through `default_book_id`, so the form shows it
    /// read-only (the client does not send read-only fields; create takes the context default).
    #[field(
        many2one,
        comodel = "sample_library.book",
        string = "Book",
        required = true,
        readonly = true
    )]
    pub book_id: Many2one<Book>,
    /// Who borrows it.
    #[field(
        many2one,
        comodel = "res.partner",
        string = "Borrower",
        required = true
    )]
    pub borrower_id: Many2one<Partner>,
    /// When it is due back.
    #[field(date, string = "Due Date", required = true)]
    pub due_date: Option<Date>,
}

fn after_days(date: Date, days: i32) -> Date {
    Date::from_days(date.days() + days)
}

fn text(value: Option<Arc<str>>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// Return the loans, which must all be out: each is `returned` today and its book available again.
async fn return_loans(loans: &RecordSet<Loan>, tx: &mut Transaction) -> Result<()> {
    if loans.is_empty() {
        return Err(Error::validation("There is no loan to return."));
    }
    for state in loans.mapped(tx, Loan::STATE).await? {
        if state.as_deref() != Some(OUT) {
            return Err(Error::validation(
                "Only a loan that is out can be returned.",
            ));
        }
    }
    let today = loans.context_today(tx)?;
    loans
        .write(
            tx,
            loans
                .vals()
                .set(Loan::STATE, "returned")
                .set(Loan::RETURN_DATE, today),
        )
        .await?;
    let books = loans.related(tx, Loan::BOOK_ID).await?;
    books
        .write(tx, books.vals().set(Book::AVAILABLE, true))
        .await
}

#[methods(Book)]
impl BookStats {
    /// The Lend button: open the Lend wizard for this book, the due date [`LOAN_DAYS`] days from
    /// today. `rpc` marks it callable from the web client (opt-in, as `#[rpc]` is on a native
    /// method).
    #[method(wire, rpc)]
    async fn lend(this: &RecordSet<Book>, tx: &mut Transaction) -> Result<Json> {
        let book = this.ensure_one()?;
        if !book.get(tx, Book::AVAILABLE).await? {
            return Err(Error::validation(format!(
                "'{}' is already on loan.",
                text(book.get(tx, Book::TITLE).await?)
            )));
        }
        let due = after_days(book.context_today(tx)?, LOAN_DAYS);
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Lend Book",
            "res_model": "sample_library.lend.wizard",
            "target": "new",
            "views": [[false, "form"]],
            "context": {
                "default_book_id": book.rec_ids()[0].get(),
                "default_due_date": due.to_string(),
            },
        }))
    }

    /// Open the caller-visible loans of this persisted book in the ordinary list/form action.
    /// The count is supplied separately by the host's authorized relation_count RPC.
    #[method(wire, rpc)]
    async fn open_loans(this: &RecordSet<Book>, tx: &mut Transaction) -> Result<Json> {
        let book = this.ensure_one()?;
        // A real field read checks owner model/field/record rights without reading child ids.
        let _title = book.get(tx, Book::TITLE).await?;
        let id = book.rec_ids()[0]
            .as_real()
            .ok_or_else(|| Error::validation("Save the book before opening loans."))?
            .get();
        Ok(json!({
            "type": "ir.actions.act_window", "name": "Loans",
            "res_model": "sample_library.loan", "target": "current",
            "views": [[false, "list"], [false, "form"]],
            "domain": [["book_id", "=", id]], "context": {"default_book_id": id},
        }))
    }

    /// The Return button of a book: return the loan that is out.
    #[method(wire, rpc)]
    async fn give_back(this: &RecordSet<Book>, tx: &mut Transaction) -> Result<bool> {
        let book = this.ensure_one()?;
        let loans = book.related(tx, Book::LOAN_IDS).await?;
        let states = loans.mapped(tx, Loan::STATE).await?;
        let out = loans.browse(
            loans
                .rec_ids()
                .iter()
                .zip(&states)
                .filter(|(_, state)| state.as_deref() == Some(OUT))
                .map(|(id, _)| id.get()),
        )?;
        return_loans(&out, tx).await?;
        Ok(true)
    }

    /// Trim the title of new books, then create them through the layers below.
    #[method(override)]
    async fn create(
        this: &RecordSet<Book>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            if let Some(Cell::Str(title)) = vals.get("title").cloned() {
                vals.insert("title".into(), Cell::Str(title.trim().into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[compute(depends = ["times_lent", "last_borrower"])]
    fn compute_lending_summary(batch: &mut Batch<'_, Book>) -> Result<()> {
        for i in batch.indices() {
            let times = batch.get(i, BookStats::TIMES_LENT)?;
            let who = batch.get(i, BookStats::LAST_BORROWER)?;
            let text = match who {
                Some(who) if times > 0 => format!("Lent {times} time(s), last to {who}"),
                _ => "Never lent".to_owned(),
            };
            batch.set(i, BookStats::LENDING_SUMMARY, text)?;
        }
        Ok(())
    }

    #[compute(depends = ["times_lent"])]
    fn compute_popular(batch: &mut Batch<'_, Book>) -> Result<()> {
        for i in batch.indices() {
            let times = batch.get(i, BookStats::TIMES_LENT)?;
            batch.set(i, BookStats::POPULAR, times >= POPULAR_AFTER)?;
        }
        Ok(())
    }
}

#[methods(LendWizard)]
impl LendWizard {
    /// The wizard's Lend button: create the loan, mark the book lent, then close the dialog with a
    /// notification.
    #[method(wire, rpc)]
    async fn action_confirm(this: &RecordSet<LendWizard>, tx: &mut Transaction) -> Result<Json> {
        let wizard = this.ensure_one()?;
        let book = wizard.related(tx, LendWizard::BOOK_ID).await?;
        if book.len() != 1 {
            return Err(Error::validation("Choose the book to lend."));
        }
        let borrower = wizard.related(tx, LendWizard::BORROWER_ID).await?;
        if borrower.len() != 1 {
            return Err(Error::validation("Choose who borrows the book."));
        }
        let today = wizard.context_today(tx)?;
        let due = wizard
            .get(tx, LendWizard::DUE_DATE)
            .await?
            .ok_or_else(|| Error::validation("Set the due date."))?;
        if due < today {
            return Err(Error::validation("The due date cannot be before today."));
        }
        let title = text(book.get(tx, Book::TITLE).await?);
        if !book.get(tx, Book::AVAILABLE).await? {
            return Err(Error::validation(format!("'{title}' is already on loan.")));
        }
        let name = text(borrower.get(tx, Partner::NAME).await?);
        let loans = wizard.env().model::<Loan>()?;
        let vals = loans
            .vals()
            .set(Loan::BOOK_ID, book.rec_ids()[0])
            .set(Loan::BORROWER_ID, borrower.rec_ids()[0])
            .set(Loan::LOAN_DATE, today)
            .set(Loan::DUE_DATE, due)
            .set(Loan::STATE, OUT);
        loans.create(tx, vec![vals]).await?;
        let times = book.get(tx, BookStats::TIMES_LENT).await? + 1;
        book.write(
            tx,
            book.vals()
                .set(Book::AVAILABLE, false)
                .set(BookStats::TIMES_LENT, times)
                .set(BookStats::LAST_BORROWER, name.as_str()),
        )
        .await?;
        Ok(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "title": "Book lent",
                "message": format!("'{title}' is lent to {name} until {due}."),
                "type": "success",
                "next": {"type": "ir.actions.act_window_close"},
            },
        }))
    }
}

#[methods(Loan)]
impl LoanRenewals {
    /// The Renew button: move the due date [`LOAN_DAYS`] days further, count the renewal, keep
    /// `note` (or a default one), and return how many renewals the loan has. A loan that is not out
    /// cannot be renewed. `note` is optional so that the form's button works.
    #[method(wire, rpc)]
    async fn renew(
        this: &RecordSet<Loan>,
        tx: &mut Transaction,
        note: Option<String>,
    ) -> Result<i32> {
        let loan = this.ensure_one()?;
        if loan.get(tx, Loan::STATE).await?.as_deref() != Some(OUT) {
            return Err(Error::validation("Only a loan that is out can be renewed."));
        }
        let from = match loan.get(tx, Loan::DUE_DATE).await? {
            Some(due) => due,
            None => loan.context_today(tx)?,
        };
        let due = after_days(from, LOAN_DAYS);
        let renewals = loan.get(tx, LoanRenewals::RENEWALS).await? + 1;
        let note = note.unwrap_or_else(|| format!("renewed until {due}"));
        // the note is written with the due date, so the `write` override does not stamp it
        loan.write(
            tx,
            loan.vals()
                .set(Loan::DUE_DATE, due)
                .set(LoanRenewals::RENEWALS, renewals)
                .set(LoanRenewals::RENEWAL_NOTE, note.as_str()),
        )
        .await?;
        Ok(renewals)
    }

    /// The Return button of a loan.
    #[method(wire, rpc)]
    async fn give_back(this: &RecordSet<Loan>, tx: &mut Transaction) -> Result<bool> {
        return_loans(this, tx).await?;
        Ok(true)
    }

    /// A loan may be renewed at most [`MAX_RENEWALS`] times.
    #[constrains("renewals")]
    async fn check_renewals(this: &RecordSet<Loan>, tx: &mut Transaction) -> Result<()> {
        for renewals in this.mapped(tx, LoanRenewals::RENEWALS).await? {
            if renewals > MAX_RENEWALS {
                return Err(Error::validation(format!(
                    "A loan can be renewed at most {MAX_RENEWALS} times (got {renewals})."
                )));
            }
        }
        Ok(())
    }

    /// A loan that was renewed is part of the library's history: it is kept. Other loans are
    /// deleted by the core `unlink`, with the caller's rights.
    #[method(override)]
    async fn unlink(this: &RecordSet<Loan>, tx: &mut Transaction, sup: Super) -> Result<()> {
        if this
            .mapped(tx, LoanRenewals::RENEWALS)
            .await?
            .iter()
            .any(|n| *n > 0)
        {
            return Err(Error::validation(
                "A renewed loan is kept for the record and cannot be deleted.",
            ));
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Editing the due date by hand leaves a trace in the renewal note, unless the caller wrote
    /// the note too; then the core `write` runs, with the caller's rights.
    #[method(override)]
    async fn write(
        this: &RecordSet<Loan>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if vals.contains("due_date") && !vals.contains("renewal_note") {
            vals.insert(
                "renewal_note".into(),
                Cell::Str("due date edited by hand".into()),
            );
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}
