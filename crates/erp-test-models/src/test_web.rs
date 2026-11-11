use erp_sdk::prelude::*;
use erp_sdk::{DefaultContext, OnchangeWarning};

erp_sdk::addon!(name = "test_web");

/// `test_orm.person`.
#[model(name = "test_web.person", description = "Person", addon = "test_web")]
pub struct Person {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub street: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(integer)]
    pub sequence: i32,
}

/// `test_orm.course`.
#[model(name = "test_web.course", description = "Course", addon = "test_web")]
pub struct Course {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person")]
    pub author_id: Option<RecId>,
    #[field(one2many, comodel = "test_web.lesson", inverse_name = "course_id")]
    pub lesson_ids: Vec<RecId>,
    #[field(reference, selection = [("test_web.lesson", "Lesson"), ("test_web.person", "Person")])]
    pub reference: Option<Arc<str>>,
    #[field(char)]
    pub m2o_reference_model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "m2o_reference_model")]
    pub m2o_reference_id: i64,
}

/// `test_orm.lesson`.
#[model(name = "test_web.lesson", description = "Lesson", addon = "test_web")]
pub struct Lesson {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.course")]
    pub course_id: Option<RecId>,
    #[field(many2one, comodel = "test_web.person")]
    pub teacher_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_web.person",
        relation = "test_web_lesson_attendee_rel",
        column1 = "lesson_id",
        column2 = "person_id"
    )]
    pub attendee_ids: Vec<RecId>,
}

/// Bookings: a `restrict` reference to persons (`web_unlink`).
#[model(name = "test_web.booking", description = "Booking", addon = "test_web")]
pub struct Booking {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person", ondelete = "restrict")]
    pub person_id: Option<RecId>,
    #[field(many2one, comodel = "test_web.room", ondelete = "restrict")]
    pub room_id: Option<RecId>,
}

/// Tickets: deleted with their person (cascade); a seat may block that (G2).
#[model(name = "test_web.ticket", description = "Ticket", addon = "test_web")]
pub struct Ticket {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person", ondelete = "cascade")]
    pub person_id: Option<RecId>,
}

/// Seats: a `restrict` reference to tickets, so deleting a person is blocked one level down.
#[model(name = "test_web.seat", description = "Seat", addon = "test_web")]
pub struct Seat {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.ticket", ondelete = "restrict")]
    pub ticket_id: Option<RecId>,
}

/// Rooms: no active field (not archivable).
#[model(name = "test_web.room", description = "Room", addon = "test_web")]
pub struct Room {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_orm.mixed` subset for `web_save_multi`.
#[model(name = "test_web.mixed", description = "Mixed", addon = "test_web")]
pub struct Mixed {
    #[field(char)]
    pub char: Option<Arc<str>>,
    #[field(integer)]
    pub integer: i32,
    #[field(float, digits = (10, 2))]
    pub number: Decimal,
}

/// `test_orm.discussion`.
#[model(
    name = "test_web.discussion",
    description = "Discussion",
    addon = "test_web"
)]
pub struct Discussion {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub note: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person")]
    pub moderator: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_web.person",
        relation = "test_web_discussion_participant_rel",
        column1 = "discussion_id",
        column2 = "person_id"
    )]
    pub participants: Vec<RecId>,
    #[field(one2many, comodel = "test_web.message", inverse_name = "discussion")]
    pub messages: Vec<RecId>,
}

#[methods(Discussion, addon = "test_web")]
impl Discussion {
    /// Moderating a discussion names it after the moderator.
    #[onchange("moderator")]
    async fn _onchange_moderator(
        this: &RecordSet<Discussion>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let moderator = this.as_records().related(tx, "moderator").await?;
        if moderator.is_empty() {
            return Ok(OnchangeResult::none());
        }
        let name = match moderator.value(tx, "name").await? {
            Value::Str(s) => format!("Moderated by {s}"),
            _ => "Moderated".to_owned(),
        };
        this.set(tx, Discussion::NAME, name.as_str()).await?;
        Ok(OnchangeResult::none())
    }

    /// The discussion's name becomes the body of every message line.
    #[onchange("name")]
    async fn _onchange_name_lines(
        this: &RecordSet<Discussion>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let name = this.get(tx, Discussion::NAME).await?;
        if name.as_deref() == Some("warn") {
            return Ok(OnchangeResult::warning("Careful", "first warning"));
        }
        if matches!(
            name.as_deref(),
            Some(
                "options-duplicate"
                    | "options-false-distinct"
                    | "options-class-distinct"
                    | "options-ordered"
            )
        ) {
            return Ok(OnchangeResult {
                warning: Some(
                    OnchangeWarning {
                        title: Some("Notice".to_owned()),
                        message: Some("first".to_owned()),
                        kind: Some("notification".to_owned()),
                        ..OnchangeWarning::default()
                    }
                    .with_options(Some("addon_notice"), None)?,
                ),
                ..OnchangeResult::none()
            });
        }
        let lines = this.as_records().related(tx, "messages").await?;
        for line in lines.iter() {
            let body = name.as_deref().map_or(Value::False, Value::str);
            line.set_value(tx, "body", body).await?;
        }
        Ok(OnchangeResult::none())
    }

    /// A second warning on the same field.
    #[onchange("name")]
    async fn _onchange_name_warning(
        this: &RecordSet<Discussion>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let name = this.get(tx, Discussion::NAME).await?;
        Ok(match name.as_deref() {
            Some("warn") => OnchangeResult::warning("Beware", "second warning"),
            Some("one warning") => OnchangeResult::warning("Careful", "only warning")
                .value("note", Value::str("noted")),
            Some(
                name @ ("options-single"
                | "options-single-timed"
                | "options-duplicate"
                | "options-false-distinct"
                | "options-class-distinct"
                | "options-ordered"),
            ) => {
                let (class_name, sticky, message) = match name {
                    "options-single" => ("addon_notice _urgent-2", Some(true), "first"),
                    "options-single-timed" => ("addon_notice _urgent-2", Some(false), "first"),
                    "options-false-distinct" => ("addon_notice", Some(false), "first"),
                    "options-class-distinct" => ("other_notice", None, "first"),
                    "options-ordered" => ("addon_notice", Some(true), "second"),
                    "options-duplicate" => (" addon_notice  ", None, "first"),
                    _ => return Err(Error::programming("Unknown warning options fixture")),
                };
                OnchangeResult {
                    warning: Some(
                        OnchangeWarning {
                            title: Some("Notice".to_owned()),
                            message: Some(message.to_owned()),
                            kind: Some("notification".to_owned()),
                            ..OnchangeWarning::default()
                        }
                        .with_options(Some(class_name), sticky)?,
                    ),
                    ..OnchangeResult::none()
                }
                .value("note", Value::str("noted"))
            }
            Some("options-invalid") => OnchangeResult {
                warning: Some(OnchangeWarning::default().with_options(Some("a:b"), None)?),
                ..OnchangeResult::none()
            },
            Some("bad key") => OnchangeResult::none().value("nope", Value::Int(1)),
            _ => OnchangeResult::none(),
        })
    }

    /// Writes the moderator's street in the database when the note says so (an onchange that
    /// must never persist).
    #[onchange("note")]
    async fn _onchange_note(
        this: &RecordSet<Discussion>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let note = this.get(tx, Discussion::NOTE).await?;
        if note.as_deref() == Some("persist") {
            let moderator = this.as_records().related(tx, "moderator").await?;
            let origin = moderator.origin();
            origin
                .set_value(tx, "street", Value::str("persisted"))
                .await?;
            tx.flush().await?;
        }
        // an explicit savepoint released inside the onchange
        if note.as_deref() == Some("nested") {
            let moderator = this.as_records().related(tx, "moderator").await?;
            let origin = moderator.origin();
            tx.savepoint(async |tx: &mut Transaction| {
                origin.set_value(tx, "street", Value::str("nested")).await
            })
            .await?;
        }
        // raw statements that change data without being INSERT/UPDATE/DELETE
        if note.as_deref() == Some("truncate") {
            tx.execute("TRUNCATE test_web_message", &[]).await?;
        }
        if note.as_deref() == Some("call") {
            tx.execute("CALL test_web_touch_people()", &[]).await?;
        }
        // error paths of the isolation (review M4)
        match note.as_deref() {
            // a server error on a read, before anything changed
            Some("bad read") => {
                tx.query("SELECT 1 / 0", &[]).await?;
            }
            // a unique violation after the savepoint was opened
            Some("duplicate") => {
                let moderator = this.as_records().related(tx, "moderator").await?;
                let id = moderator.origin().ids()[0];
                tx.execute(
                    "UPDATE test_web_person SET street = 'dup' WHERE id = $1",
                    &[&id],
                )
                .await?;
                tx.execute("INSERT INTO test_web_person (id) VALUES ($1)", &[&id])
                    .await?;
            }
            // slow enough to be cancelled from outside
            Some("slow") => {
                tx.execute("UPDATE test_web_person SET street = 'slow'", &[])
                    .await?;
                tx.query("SELECT pg_sleep(2)", &[]).await?;
            }
            // ends the server transaction behind the ORM's back: the scope's ROLLBACK TO fails
            Some("raw rollback") => {
                tx.execute("UPDATE test_web_person SET street = 'raw'", &[])
                    .await?;
                tx.batch("ROLLBACK").await?;
            }
            _ => {}
        }
        // a commit inside an onchange is refused
        if note.as_deref() == Some("commit") {
            tx.commit().await?;
        }
        // switching the transaction's ORM inside an onchange is refused
        if note.as_deref() == Some("switch orm") {
            let target = SWITCH_TARGET
                .get()
                .ok_or_else(|| erp_sdk::Error::programming("test_web::SWITCH_TARGET is not set"))?;
            tx.switch_orm(target).await?;
        }
        Ok(OnchangeResult::none())
    }
}

/// The ORM the `note = "switch orm"` onchange switches to (`Transaction::switch_orm` refusal
/// inside an isolated scope).
pub static SWITCH_TARGET: std::sync::OnceLock<erp_sdk::Orm> = std::sync::OnceLock::new();

/// Calls of [`ComputeOnchange`]'s onchange method on `foo`.
pub static FOO_ONCHANGE_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `test_orm.compute.onchange`: `bar` (readonly) and `baz` (editable) computed from `foo` and
/// `active`.
#[model(
    name = "test_web.compute_onchange",
    description = "Compute Onchange",
    addon = "test_web"
)]
pub struct ComputeOnchange {
    #[field(boolean)]
    pub active: bool,
    #[field(char)]
    pub foo: Option<Arc<str>>,
    #[field(char, compute = "_compute_bar", store)]
    pub bar: Option<Arc<str>>,
    #[field(char, compute = "_compute_baz", store, readonly = false)]
    pub baz: Option<Arc<str>>,
}

#[methods(ComputeOnchange, addon = "test_web")]
impl ComputeOnchange {
    #[compute(depends = ["foo"])]
    fn _compute_bar(batch: &mut Batch<'_, ComputeOnchange>) -> Result<()> {
        for i in batch.indices() {
            let text = batch.get(i, ComputeOnchange::FOO)?;
            batch.set(
                i,
                ComputeOnchange::BAR,
                format!("{}r", text.as_deref().unwrap_or("")),
            )?;
        }
        Ok(())
    }

    /// Assigned only while `active` (else the value stays: lead decision G1).
    #[compute(depends = ["foo", "active"])]
    fn _compute_baz(batch: &mut Batch<'_, ComputeOnchange>) -> Result<()> {
        for i in batch.indices() {
            if batch.get(i, ComputeOnchange::ACTIVE)? {
                let text = batch.get(i, ComputeOnchange::FOO)?;
                batch.set(
                    i,
                    ComputeOnchange::BAZ,
                    format!("{}z", text.as_deref().unwrap_or("")),
                )?;
            }
        }
        Ok(())
    }

    #[onchange("foo")]
    async fn _onchange_foo(
        _this: &RecordSet<ComputeOnchange>,
        _tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        FOO_ONCHANGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(OnchangeResult::none())
    }
}

fn current_user(ctx: &DefaultContext) -> Value {
    Value::Int(ctx.uid)
}

/// `test_orm.message`.
#[model(name = "test_web.message", description = "Message", addon = "test_web")]
pub struct Message {
    #[field(many2one, comodel = "test_web.discussion")]
    pub discussion: Option<RecId>,
    #[field(text)]
    pub body: Option<Arc<str>>,
    #[field(many2one, comodel = "res.users", default_fn = current_user)]
    pub author: Option<RecId>,
    #[field(char, compute = "_compute_name", store)]
    pub name: Option<Arc<str>>,
    #[field(integer, compute = "_compute_size", store)]
    pub size: i32,
    #[field(boolean)]
    pub important: bool,
}

fn text_of(value: Value) -> String {
    match value {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

#[methods(Message, addon = "test_web")]
impl Message {
    #[compute(depends = ["discussion", "author"])]
    async fn _compute_name(
        this: &RecordSet<Message>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Message>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let discussion = record.as_records().related(tx, "discussion").await?;
            let author = record.as_records().related(tx, "author").await?;
            let d = if discussion.is_empty() {
                String::new()
            } else {
                text_of(discussion.value(tx, "name").await?)
            };
            let a = if author.is_empty() {
                String::new()
            } else {
                text_of(author.value(tx, "name").await?)
            };
            out.set(i, Message::NAME, format!("[{d}] {a}"))?;
        }
        Ok(())
    }

    #[compute(depends = ["body"])]
    fn _compute_size(batch: &mut Batch<'_, Message>) -> Result<()> {
        for i in batch.indices() {
            let body = batch.get(i, Message::BODY)?;
            let size = body.as_deref().map_or(0, |b| b.chars().count());
            batch.set(i, Message::SIZE, i32::try_from(size).unwrap_or(i32::MAX))?;
        }
        Ok(())
    }
}

/// `test_orm.multi`: a form whose lines have x2many fields of their own.
#[model(name = "test_web.multi", description = "Multi", addon = "test_web")]
pub struct Multi {
    #[field(char, related = "partner.name")]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person")]
    pub partner: Option<RecId>,
    #[field(one2many, comodel = "test_web.multi_line", inverse_name = "multi")]
    pub lines: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_web.multi_tag",
        relation = "test_web_multi_tag_rel",
        column1 = "multi_id",
        column2 = "tag_id"
    )]
    pub tags: Vec<RecId>,
    /// The amounts of the lines' sub-lines: a field computed two x2many levels down.
    #[field(integer, compute = "_compute_total")]
    pub total: i32,
    /// The tags of the lines, counted per line: a field computed over the lines' many2many
    /// values (nested tag commands reach the form's fields).
    #[field(integer, compute = "_compute_tag_total")]
    pub tag_total: i32,
    /// Changes the lines' sub-lines through its onchange method ([`Multi::BONUS_ADD_LINE`],
    /// negative values, or an amount added to every sub-line).
    #[field(integer)]
    pub bonus: i32,
}

impl Multi {
    /// The `bonus` that adds a sub-line named "bonus" (amount 100) to every line.
    pub const BONUS_ADD_LINE: i32 = 100;
    /// The `bonus` that links to every sub-line of the k-th line the k-th label of the context's
    /// `link_labels` (a list of label ids): records no snapshot has read yet, one per line.
    pub const BONUS_LINK_LABELS: i32 = 200;
}

/// Sum of `amount` over `records`, overflow refused.
async fn amount_sum(tx: &mut Transaction, records: &Records, model: &str) -> Result<i32> {
    let mut total: i32 = 0;
    for value in records.values(tx, "amount").await? {
        let Value::Int(amount) = value else {
            return Err(Error::programming(format!(
                "{model}: amount {value} is not an integer"
            )));
        };
        // an amount outside i32 is the overflow error below, not a dropped value
        total = i32::try_from(amount)
            .ok()
            .and_then(|a| total.checked_add(a))
            .ok_or_else(|| Error::validation(format!("{model}: the amounts' total overflows")))?;
    }
    Ok(total)
}

#[methods(Multi, addon = "test_web")]
impl Multi {
    #[compute(depends = ["lines.sub_ids.amount"])]
    async fn _compute_total(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Multi>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let lines = record.as_records().related(tx, "lines").await?;
            let subs = lines.related(tx, "sub_ids").await?;
            let total = amount_sum(tx, &subs, "test_web.multi").await?;
            out.set(i, Multi::TOTAL, total)?;
        }
        Ok(())
    }

    #[compute(depends = ["lines.tags"])]
    async fn _compute_tag_total(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Multi>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let lines = record.as_records().related(tx, "lines").await?;
            let mut total: usize = 0;
            for line in lines.iter() {
                let tags = line.related(tx, "tags").await?;
                total = total.checked_add(tags.len()).ok_or_else(|| {
                    Error::validation("test_web.multi: the tags' total overflows")
                })?;
            }
            out.set(i, Multi::TAG_TOTAL, link_count("test_web.multi", total)?)?;
        }
        Ok(())
    }

    #[onchange("name")]
    async fn _onchange_name(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let name = this.as_records().value(tx, "name").await?;
        let lines = this.as_records().related(tx, "lines").await?;
        lines.set_value(tx, "name", name).await?;
        Ok(OnchangeResult::none())
    }

    #[onchange("partner")]
    async fn _onchange_partner(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let partner = this.as_records().value(tx, "partner").await?;
        let lines = this.as_records().related(tx, "lines").await?;
        lines.set_value(tx, "partner", partner).await?;
        Ok(OnchangeResult::none())
    }

    #[onchange("tags")]
    async fn _onchange_tags(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let tags = this.as_records().related(tx, "tags").await?;
        let lines = this.as_records().related(tx, "lines").await?;
        for line in lines.iter() {
            let all = line.related(tx, "tags").await?.union(&tags)?;
            line.assign(tx, "tags", &all).await?;
        }
        Ok(OnchangeResult::none())
    }

    /// Every line's sub-lines change: [`Multi::BONUS_ADD_LINE`] adds a sub-line "bonus" of amount
    /// 100 to every line, a negative bonus removes every line's first sub-line, any other bonus is
    /// added to every sub-line's amount.
    #[onchange("bonus")]
    async fn _onchange_bonus(
        this: &RecordSet<Multi>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let bonus = this.get(tx, Multi::BONUS).await?;
        let lines = this.as_records().related(tx, "lines").await?;
        let subs_model = tx.env().records("test_web.multi_sub")?;
        if bonus == Multi::BONUS_LINK_LABELS {
            let Some(Value::List(ids)) = this.as_records().env().context().get("link_labels")
            else {
                return Err(Error::validation(
                    "test_web.multi: BONUS_LINK_LABELS needs the context key 'link_labels'",
                ));
            };
            let labels_model = tx.env().records("test_web.multi_label")?;
            for (line, id) in lines.iter().zip(ids.iter()) {
                let Value::Int(id) = id else {
                    return Err(Error::validation(format!(
                        "test_web.multi: label id {id} is not an integer"
                    )));
                };
                let label = labels_model.browse([*id])?;
                let subs = line.related(tx, "sub_ids").await?;
                for sub in subs.iter() {
                    sub.write(
                        tx,
                        RawVals::new().set_commands("labels", [Command::Link(label.rec_ids()[0])]),
                    )
                    .await?;
                }
            }
            return Ok(OnchangeResult::none());
        }
        for line in lines.iter() {
            let subs = line.related(tx, "sub_ids").await?;
            if bonus == Multi::BONUS_ADD_LINE {
                let vals = RawVals::new()
                    .set_cell("name", Cell::Str("bonus".into()))
                    .set_cell("amount", Cell::Int(Multi::BONUS_ADD_LINE));
                let sub = tx.new_record(&subs_model, vals, None, None).await?;
                line.write(
                    tx,
                    RawVals::new().set_commands("sub_ids", [Command::Link(sub.rec_ids()[0])]),
                )
                .await?;
            } else if bonus < 0 {
                if let Some(first) = subs.rec_ids().first() {
                    line.write(
                        tx,
                        RawVals::new().set_commands("sub_ids", [Command::Delete(*first)]),
                    )
                    .await?;
                }
            } else {
                for sub in subs.iter() {
                    let amount = match sub.value(tx, "amount").await? {
                        Value::Int(a) => a,
                        other => {
                            return Err(Error::programming(format!(
                                "test_web.multi_sub: amount {other} is not an integer"
                            )));
                        }
                    };
                    let amount = amount.checked_add(i64::from(bonus)).ok_or_else(|| {
                        Error::validation("test_web.multi_sub: the amount overflows")
                    })?;
                    sub.set_value(tx, "amount", Value::Int(amount)).await?;
                }
            }
        }
        Ok(OnchangeResult::none())
    }
}

#[model(
    name = "test_web.multi_line",
    description = "Multi line",
    addon = "test_web"
)]
pub struct MultiLine {
    #[field(many2one, comodel = "test_web.multi", ondelete = "cascade")]
    pub multi: Option<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_web.person")]
    pub partner: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_web.multi_tag",
        relation = "test_web_multi_line_tag_rel",
        column1 = "line_id",
        column2 = "tag_id"
    )]
    pub tags: Vec<RecId>,
    #[field(one2many, comodel = "test_web.multi_sub", inverse_name = "line_id")]
    pub sub_ids: Vec<RecId>,
    /// The sub-lines' amounts.
    #[field(integer, compute = "_compute_amount")]
    pub amount: i32,
    /// A stored count of the sub-lines: a stored dependent of `sub_ids` (an onchange must not
    /// recompute or write it).
    #[field(integer, compute = "_compute_sub_count", store)]
    pub sub_count: i32,
}

#[methods(MultiLine, addon = "test_web")]
impl MultiLine {
    #[compute(depends = ["sub_ids"])]
    async fn _compute_sub_count(
        this: &RecordSet<MultiLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MultiLine>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let subs = record.as_records().related(tx, "sub_ids").await?;
            let count = i32::try_from(subs.len()).map_err(|_| {
                Error::validation("test_web.multi_line: the sub-lines do not fit a count")
            })?;
            out.set(i, MultiLine::SUB_COUNT, count)?;
        }
        Ok(())
    }

    #[compute(depends = ["sub_ids.amount"])]
    async fn _compute_amount(
        this: &RecordSet<MultiLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MultiLine>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let subs = record.as_records().related(tx, "sub_ids").await?;
            let total = amount_sum(tx, &subs, "test_web.multi_line").await?;
            out.set(i, MultiLine::AMOUNT, total)?;
        }
        Ok(())
    }
}

// ----- the mirror many2many of in-memory lines ----------------------

/// A count of linked records as an integer field value (one that does not fit is an error).
fn link_count(model: &str, count: usize) -> Result<i32> {
    i32::try_from(count).map_err(|_| {
        Error::validation(format!(
            "{model}: {count} links do not fit an integer field"
        ))
    })
}

/// A board listing categories; each category lists its boards through the mirror `board_ids`.
#[model(name = "test_web.board", description = "Board", addon = "test_web")]
pub struct Board {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_web.category",
        relation = "test_web_board_category_rel",
        column1 = "board_id",
        column2 = "category_id"
    )]
    pub category_ids: X2many<Category>,
    /// A second many2many to categories, on its own relation table (no mirror).
    #[field(
        many2many,
        comodel = "test_web.category",
        relation = "test_web_board_featured_rel",
        column1 = "board_id",
        column2 = "category_id"
    )]
    pub featured_ids: X2many<Category>,
    /// The boards listed by the board's categories, counted per category.
    #[field(integer, compute = "_compute_category_links")]
    pub category_links: i32,
    /// The same over the featured categories (their `board_ids` follow `category_ids`).
    #[field(integer, compute = "_compute_featured_links")]
    pub featured_links: i32,
}

/// The boards listed by `categories`, counted per category.
fn board_links(batch: &Batch<'_, Board>, categories: &X2many<Category>) -> Result<i32> {
    let mut links: usize = 0;
    for boards in batch.values_of(Category::BOARD_IDS, categories.ids())? {
        links = links.checked_add(boards.len()).ok_or_else(|| {
            Error::validation("test_web.board: the number of category links overflows")
        })?;
    }
    link_count("test_web.board", links)
}

#[methods(Board, addon = "test_web")]
impl Board {
    #[compute(depends = ["category_ids.board_ids"])]
    fn _compute_category_links(batch: &mut Batch<'_, Board>) -> Result<()> {
        for i in batch.indices() {
            let categories = batch.get(i, Board::CATEGORY_IDS)?;
            let links = board_links(batch, &categories)?;
            batch.set(i, Board::CATEGORY_LINKS, links)?;
        }
        Ok(())
    }

    #[compute(depends = ["featured_ids.board_ids"])]
    fn _compute_featured_links(batch: &mut Batch<'_, Board>) -> Result<()> {
        for i in batch.indices() {
            let categories = batch.get(i, Board::FEATURED_IDS)?;
            let links = board_links(batch, &categories)?;
            batch.set(i, Board::FEATURED_LINKS, links)?;
        }
        Ok(())
    }
}

/// A sub-line of a [`MultiLine`] (a one2many inside the one2many), which may have sub-lines of its
/// own (`child_ids`, any depth).
#[model(
    name = "test_web.multi_sub",
    description = "Multi sub-line",
    addon = "test_web"
)]
pub struct MultiSub {
    #[field(many2one, comodel = "test_web.multi_line", ondelete = "cascade")]
    pub line_id: Option<RecId>,
    #[field(many2one, comodel = "test_web.multi_sub", ondelete = "cascade")]
    pub parent_id: Option<RecId>,
    /// With a domain (every amount the tests give is not negative), so that linking children
    /// evaluates it on the linked records.
    #[field(
        one2many,
        comodel = "test_web.multi_sub",
        inverse_name = "parent_id",
        domain = [("amount", ">=", 0)]
    )]
    pub child_ids: Vec<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub amount: i32,
    /// Archivable: an archived sub-line is not part of its line's value.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Labels of the sub-line (a many2many whose mirror is [`MultiLabel::sub_ids`]).
    #[field(
        many2many,
        comodel = "test_web.multi_label",
        relation = "test_web_multi_sub_label_rel",
        column1 = "sub_id",
        column2 = "label_id"
    )]
    pub labels: Vec<RecId>,
    /// The name of the sub-line's line: an in-memory sub-line reads its in-memory line.
    #[field(char, compute = "_compute_line_name")]
    pub line_name: Option<Arc<str>>,
}

#[methods(MultiSub, addon = "test_web")]
impl MultiSub {
    #[compute(depends = ["line_id.name"])]
    async fn _compute_line_name(
        this: &RecordSet<MultiSub>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MultiSub>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let line = record.as_records().related(tx, "line_id").await?;
            let name = if line.is_empty() {
                Value::False
            } else {
                line.value(tx, "name").await?
            };
            let name: Option<Arc<str>> = match name {
                Value::Str(s) => Some(s),
                Value::False => None,
                other => {
                    return Err(Error::programming(format!(
                        "MultiSub: line name {other} is not a string"
                    )));
                }
            };
            out.set(i, MultiSub::LINE_NAME, name)?;
        }
        Ok(())
    }
}

/// A label of sub-lines, archivable; it lists the sub-lines that carry it (the mirror of
/// [`MultiSub::labels`]) and counts them.
#[model(
    name = "test_web.multi_label",
    description = "Multi label",
    addon = "test_web"
)]
pub struct MultiLabel {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(
        many2many,
        comodel = "test_web.multi_sub",
        relation = "test_web_multi_sub_label_rel",
        column1 = "label_id",
        column2 = "sub_id"
    )]
    pub sub_ids: Vec<RecId>,
    /// The sub-lines that carry the label.
    #[field(integer, compute = "_compute_sub_count")]
    pub sub_count: i32,
}

#[methods(MultiLabel, addon = "test_web")]
impl MultiLabel {
    #[compute(depends = ["sub_ids"])]
    async fn _compute_sub_count(
        this: &RecordSet<MultiLabel>,
        tx: &mut Transaction,
        out: &mut Assign<'_, MultiLabel>,
    ) -> Result<()> {
        for (i, record) in this.iter().enumerate() {
            let subs = record.as_records().related(tx, "sub_ids").await?;
            let count = i32::try_from(subs.len()).map_err(|_| {
                Error::validation("test_web.multi_label: the sub-lines do not fit a count")
            })?;
            out.set(i, MultiLabel::SUB_COUNT, count)?;
        }
        Ok(())
    }
}

/// `test_orm.category` subset: the boards listing it (the mirror of `test_web.board.category_ids`).
#[model(
    name = "test_web.category",
    description = "Category",
    addon = "test_web"
)]
pub struct Category {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_web.board",
        relation = "test_web_board_category_rel",
        column1 = "category_id",
        column2 = "board_id"
    )]
    pub board_ids: X2many<Board>,
    #[field(integer, compute = "_compute_board_count")]
    pub board_count: i32,
}

#[methods(Category, addon = "test_web")]
impl Category {
    #[compute(depends = ["board_ids"])]
    fn _compute_board_count(batch: &mut Batch<'_, Category>) -> Result<()> {
        for i in batch.indices() {
            let boards = batch.get(i, Category::BOARD_IDS)?;
            batch.set(
                i,
                Category::BOARD_COUNT,
                link_count("test_web.category", boards.len())?,
            )?;
        }
        Ok(())
    }
}

/// A note on a board, delegating to it (`_inherits`): its form edits the board's categories.
#[model(
    name = "test_web.board_note",
    description = "Board note",
    inherits = [("test_web.board", "board_id")],
    addon = "test_web"
)]
pub struct BoardNote {
    #[field(many2one, comodel = "test_web.board", required, ondelete = "cascade")]
    pub board_id: Many2one<Board>,
    #[field(char)]
    pub note: Option<Arc<str>>,
}

/// `test_orm.multi.tag`.
#[model(name = "test_web.multi_tag", description = "Tag", addon = "test_web")]
pub struct MultiTag {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_compute_label")]
    pub label: Option<Arc<str>>,
}

#[methods(MultiTag, addon = "test_web")]
impl MultiTag {
    #[compute(depends = ["name"], depends_context = ["special_tag"])]
    fn _compute_label(batch: &mut Batch<'_, MultiTag>) -> Result<()> {
        let special = batch
            .env()
            .context()
            .get("special_tag")
            .is_some_and(|v| v.is_truthy());
        for i in batch.indices() {
            let name = batch.get(i, MultiTag::NAME)?;
            let label = name.as_deref().map(|n| {
                if special {
                    Arc::<str>::from(format!("{n}!"))
                } else {
                    Arc::<str>::from(n)
                }
            });
            batch.set(i, MultiTag::LABEL, label)?;
        }
        Ok(())
    }
}

/// `test_orm.move`: computed fields over its own tag and over the payments delegating to it
/// .
#[model(name = "test_web.move", description = "Move", addon = "test_web")]
pub struct Move {
    #[field(many2one, comodel = "test_web.multi_tag")]
    pub tag_id: Many2one<MultiTag>,
    #[field(char, related = "tag_id.name")]
    pub tag_name: Option<Arc<str>>,
    #[field(integer)]
    pub tag_repeat: i32,
    #[field(char, compute = "_compute_tag_string")]
    pub tag_string: Option<Arc<str>>,
    #[field(one2many, comodel = "test_web.payment", inverse_name = "move_id")]
    pub payment_ids: X2many<Payment>,
    #[field(integer, compute = "_compute_payment_amount")]
    pub payment_amount: i32,
    #[field(
        one2many,
        comodel = "test_web.payment_note",
        inverse_name = "move_ref_id"
    )]
    pub note_ids: X2many<PaymentNote>,
}

/// The longest `tag_string` the fixture builds: the repeat count comes from the client, so the
/// string is bounded instead of allocating whatever `tag_name * tag_repeat` asks for.
pub const TAG_STRING_MAX_BYTES: usize = 1 << 20;

#[methods(Move, addon = "test_web")]
impl Move {
    #[compute(depends = ["tag_name", "tag_repeat"])]
    fn _compute_tag_string(batch: &mut Batch<'_, Move>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Move::TAG_NAME)?;
            let name = name.as_deref().unwrap_or("");
            let repeat = batch.get(i, Move::TAG_REPEAT)?;
            // Python's `str * n` is the empty string for a negative `n`, as for zero
            let repeat = usize::try_from(repeat.max(0)).map_err(|_| {
                Error::validation(format!(
                    "test_web.move: tag_repeat {repeat} is out of range"
                ))
            })?;
            let bytes = name
                .len()
                .checked_mul(repeat)
                .filter(|b| *b <= TAG_STRING_MAX_BYTES)
                .ok_or_else(|| {
                    Error::validation(format!(
                        "test_web.move: tag_string of {} bytes repeated {repeat} times is \
                         longer than {TAG_STRING_MAX_BYTES} bytes",
                        name.len()
                    ))
                })?;
            let mut out = String::with_capacity(bytes);
            for _ in 0..repeat {
                out.push_str(name);
            }
            batch.set(i, Move::TAG_STRING, out)?;
        }
        Ok(())
    }

    #[compute(depends = ["payment_ids.amount"])]
    fn _compute_payment_amount(batch: &mut Batch<'_, Move>) -> Result<()> {
        for i in batch.indices() {
            let payments = batch.get(i, Move::PAYMENT_IDS)?;
            let mut total: i32 = 0;
            for amount in batch.values_of(Payment::AMOUNT, payments.ids())? {
                total = total.checked_add(amount).ok_or_else(|| {
                    Error::validation("test_web.move: the payments' total amount overflows")
                })?;
            }
            batch.set(i, Move::PAYMENT_AMOUNT, total)?;
        }
        Ok(())
    }
}

/// `test_orm.payment`, delegating to a move.
#[model(
    name = "test_web.payment",
    description = "Payment",
    inherits = [("test_web.move", "move_id")],
    addon = "test_web"
)]
pub struct Payment {
    #[field(many2one, comodel = "test_web.move", required, ondelete = "cascade")]
    pub move_id: Many2one<Move>,
    #[field(integer)]
    pub amount: i32,
    #[field(integer)]
    pub repeat_hint: i32,
    #[field(integer)]
    pub previous_move_amount: i32,
}

#[methods(Payment, addon = "test_web")]
impl Payment {
    #[onchange("repeat_hint")]
    async fn _onchange_repeat_hint(
        this: &RecordSet<Payment>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let hint = this.get(tx, Payment::REPEAT_HINT).await?;
        this.as_records()
            .set_value(tx, "tag_repeat", Value::Int(i64::from(hint)))
            .await?;
        Ok(OnchangeResult::none())
    }

    #[onchange("move_id")]
    async fn _onchange_move_id(
        this: &RecordSet<Payment>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let origin = this.origin();
        if origin.as_records().is_empty() {
            // a new payment has no saved move to report on
            return Ok(OnchangeResult::none());
        }
        let saved_move =
            RecordSet::<Move>::try_from(origin.as_records().related(tx, "move_id").await?)?;
        let moves = tx.env().model::<Move>()?;
        // the in-memory record of the saved move: the same record the onchange made its parent
        let in_memory = moves
            .new_with(tx, moves.vals(), Some(&saved_move), None)
            .await?;
        let total = in_memory.get(tx, Move::PAYMENT_AMOUNT).await?;
        this.set(tx, Payment::PREVIOUS_MOVE_AMOUNT, total).await?;
        Ok(OnchangeResult::none())
    }
}

#[model(
    name = "test_web.payment_note",
    description = "Payment note",
    inherits = [("test_web.payment", "payment_id")],
    addon = "test_web"
)]
pub struct PaymentNote {
    #[field(many2one, comodel = "test_web.payment", required, ondelete = "cascade")]
    pub payment_id: Many2one<Payment>,
    #[field(char)]
    pub note: Option<Arc<str>>,
    /// The move listing the note (its `note_ids`), independent of the payment's move.
    #[field(many2one, comodel = "test_web.move")]
    pub move_ref_id: Many2one<Move>,
    #[field(integer)]
    pub old_move_amount: i32,
}

#[methods(PaymentNote, addon = "test_web")]
impl PaymentNote {
    #[onchange("move_id")]
    async fn _onchange_note_move(
        this: &RecordSet<PaymentNote>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let origin = this.origin();
        if origin.as_records().is_empty() {
            // a new note has no saved move to report on
            return Ok(OnchangeResult::none());
        }
        let saved_payment = origin.as_records().related(tx, "payment_id").await?;
        let saved_move = RecordSet::<Move>::try_from(saved_payment.related(tx, "move_id").await?)?;
        let moves = tx.env().model::<Move>()?;
        // the in-memory record of the saved move: the same record the onchange made the note's
        // grandparent
        let in_memory = moves
            .new_with(tx, moves.vals(), Some(&saved_move), None)
            .await?;
        let total = in_memory.get(tx, Move::PAYMENT_AMOUNT).await?;
        this.set(tx, PaymentNote::OLD_MOVE_AMOUNT, total).await?;
        Ok(OnchangeResult::none())
    }
}

// ----- an `_inherits` parent with a group-restricted first column (review) -----------

#[model(name = "test_web.vault", description = "Vault", addon = "test_web")]
pub struct Vault {
    #[field(char, groups = "base.group_system")]
    pub secret: Option<Arc<str>>,
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A record delegating to a vault.
#[model(
    name = "test_web.vault_entry",
    description = "Vault entry",
    inherits = [("test_web.vault", "vault_id")],
    addon = "test_web"
)]
pub struct VaultEntry {
    #[field(many2one, comodel = "test_web.vault", required, ondelete = "cascade")]
    pub vault_id: Many2one<Vault>,
    #[field(char)]
    pub label: Option<Arc<str>>,
}

/// Category target model with hierarchy (`test_search_panel.category_target_model`).
#[model(
    name = "test_search_panel.category_target_model",
    description = "Category target model",
    order = "name",
    parent_name = "parent_name_id",
    addon = "test_web"
)]
pub struct SearchPanelCategoryTargetModel {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_search_panel.category_target_model")]
    pub parent_name_id: Option<RecId>,
}

/// Category target model without parent_name (`test_search_panel.category_target_model_no_parent_name`).
#[model(
    name = "test_search_panel.category_target_model_no_parent_name",
    description = "Category target model without parent_name",
    order = "id desc",
    addon = "test_web"
)]
pub struct SearchPanelCategoryTargetModelNoParentName {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// Filter target model (`test_search_panel.filter_target_model`).
#[model(
    name = "test_search_panel.filter_target_model",
    description = "Filter target model",
    order = "name",
    addon = "test_web"
)]
pub struct SearchPanelFilterTargetModel {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection = [("cool", "Cool"), ("unknown", "Unknown"), ("quoted\"é", "Quoted Unicode")])]
    pub status: Option<Arc<str>>,
    #[field(char)]
    pub color: Option<Arc<str>>,
    #[field(many2one, comodel = "test_search_panel.category_target_model")]
    pub folder_id: Option<RecId>,
}

/// Source model for search panel tests (`test_search_panel.source_model`).
#[model(
    name = "test_search_panel.source_model",
    description = "Source model",
    addon = "test_web"
)]
pub struct SearchPanelSourceModel {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection = [("a", "A"), ("b", "B")])]
    pub state: Option<Arc<str>>,
    #[field(many2one, comodel = "test_search_panel.category_target_model")]
    pub folder_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_search_panel.category_target_model",
        relation = "test_sp_source_categ_m2m_rel"
    )]
    pub folder_ids: Vec<RecId>,
    #[field(
        many2one,
        comodel = "test_search_panel.category_target_model_no_parent_name"
    )]
    pub categ_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_search_panel.filter_target_model",
        relation = "test_sp_source_filter_m2m_rel"
    )]
    pub tag_ids: Vec<RecId>,
    #[field(many2one, comodel = "test_search_panel.filter_target_model")]
    pub tag_id: Option<RecId>,
}
