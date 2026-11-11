//! `mail.followers`: which partner follows which thread record, and for which subtypes.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;

use crate::subtype::{DefaultSubtypes, MailMessageSubtype};

/// One partner following one record. `res_model`/`res_id` carry no foreign key; deleting a thread
/// deletes its followers through the `mail.thread` `unlink` override.
#[model(
    name = "mail.followers",
    description = "Document Follower",
    addon = "mail",
    company_access = unrestricted(reason = "follower rows are reached through the followed record's own access; the reference ERP's mail.followers has no company rule"),
    keys = [key(
        name = "_mail_followers_res_partner_res_model_id_uniq",
        fields = ["res_model", "res_id", "partner_id"],
        message = "A partner can follow a document only once."
    )]
)]
pub struct MailFollowers {
    /// Model of the followed record.
    #[field(
        char,
        required = true,
        index = true,
        string = "Related Document Model Name"
    )]
    pub res_model: Option<Arc<str>>,
    /// Id of the followed record.
    #[field(
        many2one_reference,
        model_field = "res_model",
        index = true,
        string = "Related Document ID"
    )]
    pub res_id: i64,
    /// The follower.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        index = true,
        ondelete = "cascade",
        string = "Related Partner"
    )]
    pub partner_id: Option<RecId>,
    /// Subtypes the partner receives.
    #[field(
        many2many,
        comodel = "mail.message.subtype",
        relation = "mail_followers_mail_message_subtype_rel",
        column1 = "mail_followers_id",
        column2 = "mail_message_subtype_id",
        string = "Subtype"
    )]
    pub subtype_ids: X2many<MailMessageSubtype>,
    /// The partner's name.
    #[field(char, related = "partner_id.name")]
    pub name: Option<Arc<str>>,
    /// The partner's email.
    #[field(char, related = "partner_id.email")]
    pub email: Option<Arc<str>>,
    /// Whether the partner is active.
    #[field(boolean, related = "partner_id.active", string = "Is Active")]
    pub is_active: bool,
}

/// What [`subscriptions`] found: the current user's partner and the matching follower rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscriptions {
    /// The partner of the environment's user (`None` when the user has none).
    pub(crate) user_partner: Option<i64>,
    /// Ids of the followers of `model`/`res_ids` among the partners asked for, in id order.
    pub(crate) followers: Vec<i64>,
}

/// Privileged: callers check access on the thread before acting on
/// the result. Pending writes are flushed first, so the statement sees them.
///
/// # Errors
/// Database errors; a row the statement cannot decode.
pub(crate) async fn subscriptions(
    tx: &mut Transaction,
    model: &str,
    res_ids: &[i64],
    partner_ids: &[i64],
    uid: i64,
) -> Result<Subscriptions> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT (SELECT u.partner_id::int8 FROM res_users u WHERE u.id = $4::int8), \
             ARRAY(SELECT f.id::int8 FROM mail_followers f WHERE f.res_model = $1::text \
             AND f.res_id = ANY($2::int8[]) AND f.partner_id = ANY($3::int8[]) ORDER BY f.id)",
            &[&model, &res_ids, &partner_ids, &uid],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant("the follower lookup returned no row"))?;
    let user_partner: Option<i64> = row
        .try_get(0)
        .map_err(|error| Error::db(format!("unexpected user partner value: {error}")))?;
    let followers: Vec<i64> = row
        .try_get(1)
        .map_err(|error| Error::db(format!("unexpected follower ids: {error}")))?;
    Ok(Subscriptions {
        user_partner,
        followers,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingPolicy {
    /// Leave the follower untouched.
    Skip,
    /// Make the follower's subtypes exactly the given ones (adds the missing, removes the others).
    Replace,
    /// Add the missing subtypes, never remove one.
    Update,
}

/// The subtypes [`insert_followers`] gives the partners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubtypeChoice {
    /// The defaults of the model: all of them for internal partners, the external ones for
    /// partners that are portal or public users (`partner_share`).
    Defaults,
    /// These subtype ids (already validated) for every partner.
    Explicit(Vec<i64>),
}

/// Ids in `cell`, an x2many value.
fn ids_of(cell: Cell, what: &str) -> Result<Vec<i64>> {
    match cell {
        Cell::Ids(ids) => Ok(ids.iter().map(|id| id.get()).collect()),
        other => Err(Error::invariant(format!(
            "{what} is not an id list: {other:?}"
        ))),
    }
}

/// A real record id out of `cell`.
fn id_of(cell: &Cell, what: &str) -> Result<i64> {
    match cell {
        Cell::Id(id) => Ok(*id),
        other => Err(Error::invariant(format!("{what} is not an id: {other:?}"))),
    }
}

/// Subtype ids as the `RecId`s of a command.
fn rec_ids(ids: impl IntoIterator<Item = i64>) -> Result<Vec<RecId>> {
    ids.into_iter()
        .map(|id| {
            RecId::from_real(id)
                .ok_or_else(|| Error::invariant(format!("subtype id {id} is not a real id")))
        })
        .collect()
}

/// Subtype
/// links are written through the ORM, because the upsert refuses x2many values.
///
/// # Errors
/// Access and database errors; `Invariant` for a row the ORM returns in an unexpected shape.
pub(crate) async fn insert_followers(
    tx: &mut Transaction,
    records: &Records,
    partner_ids: &[i64],
    choice: &SubtypeChoice,
    policy: ExistingPolicy,
) -> Result<()> {
    if records.is_empty() || partner_ids.is_empty() {
        return Ok(());
    }
    let env = records.env().clone();
    let model = records.model_name().to_owned();
    let mut res_ids = records.ids();
    res_ids.sort_unstable();
    res_ids.dedup();
    // the subtypes of each partner
    let mut wanted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    match choice {
        SubtypeChoice::Explicit(ids) => {
            for partner in partner_ids {
                wanted.insert(*partner, ids.clone());
            }
        }
        SubtypeChoice::Defaults => {
            let defaults: DefaultSubtypes = env
                .records("mail.message.subtype")?
                .call(tx, "default_subtypes", (model.clone(),))
                .await?;
            // A partner is internal when it has an active user that is not a share user.
            // Derived
            // from `res.users.share` here because base's stored `res.partner.partner_share` is
            // still a placeholder (true for every non-superuser partner).
            let internal_users = env
                .records("res.users")?
                .sudo(true)
                .with_context([("active_test", Value::Bool(false))]);
            let of_partners = Domain::condition("partner_id", Operator::In, partner_ids.to_vec())?;
            let internal_user = of_partners.clone()
                & Domain::condition("active", Operator::Eq, true)?
                & Domain::condition("share", Operator::Eq, false)?;
            let superuser =
                of_partners & Domain::condition("id", Operator::Eq, erp_sdk::SUPERUSER_ID.get())?;
            let users = internal_users
                .search(tx, &(internal_user | superuser))
                .await?;
            let mut internal: BTreeSet<i64> = BTreeSet::new();
            for cell in users.cells(tx, "partner_id").await? {
                internal.insert(id_of(&cell, "an internal user's partner_id")?);
            }
            for partner in partner_ids {
                let external = !internal.contains(partner);
                let ids = if external {
                    defaults.external()
                } else {
                    defaults.all()
                };
                wanted.insert(*partner, ids.to_vec());
            }
        }
    }
    // the follower rows that exist already: (res_id, partner) -> (row id, its subtypes)
    let followers = env.records("mail.followers")?.sudo(true);
    let domain = Domain::condition("res_model", Operator::Eq, model.as_str())?
        & Domain::condition("res_id", Operator::In, res_ids.clone())?
        & Domain::condition("partner_id", Operator::In, partner_ids.to_vec())?;
    let rows = followers.search(tx, &domain).await?;
    let row_ids = rows.ids();
    let row_res = rows.cells(tx, "res_id").await?;
    let row_partners = rows.cells(tx, "partner_id").await?;
    let row_subtypes = rows.cells(tx, "subtype_ids").await?;
    let mut existing: BTreeMap<(i64, i64), (i64, BTreeSet<i64>)> = BTreeMap::new();
    for (((row, res), partner), subtypes) in row_ids
        .iter()
        .zip(&row_res)
        .zip(&row_partners)
        .zip(row_subtypes)
    {
        existing.insert(
            (
                id_of(res, "a follower's res_id")?,
                id_of(partner, "a follower's partner_id")?,
            ),
            (
                *row,
                ids_of(subtypes, "a follower's subtypes")?
                    .into_iter()
                    .collect(),
            ),
        );
    }
    let mut creates = Vec::new();
    // follower rows grouped by the one command that changes them
    let mut replaces: BTreeMap<Vec<i64>, Vec<i64>> = BTreeMap::new();
    let mut additions: BTreeMap<Vec<i64>, Vec<i64>> = BTreeMap::new();
    for res_id in &res_ids {
        for (partner, subtypes) in &wanted {
            let target: BTreeSet<i64> = subtypes.iter().copied().collect();
            match existing.get(&(*res_id, *partner)) {
                None => creates.push(
                    RawVals::new()
                        .set_cell("res_model", Cell::Str(model.as_str().into()))
                        .set_cell("res_id", Cell::Id(*res_id))
                        .set_cell("partner_id", Cell::Id(*partner))
                        .set_commands("subtype_ids", [Command::set(rec_ids(target)?)]),
                ),
                Some((row, current)) => match policy {
                    ExistingPolicy::Skip => {}
                    ExistingPolicy::Replace => {
                        if *current != target {
                            replaces
                                .entry(target.into_iter().collect())
                                .or_default()
                                .push(*row);
                        }
                    }
                    ExistingPolicy::Update => {
                        let missing: Vec<i64> = target.difference(current).copied().collect();
                        if !missing.is_empty() {
                            additions.entry(missing).or_default().push(*row);
                        }
                    }
                },
            }
        }
    }
    if !creates.is_empty() {
        followers.create(tx, creates).await?;
    }
    for (subtypes, rows) in replaces {
        followers
            .browse(rows)?
            .write(
                tx,
                RawVals::new().set_commands("subtype_ids", [Command::set(rec_ids(subtypes)?)]),
            )
            .await?;
    }
    for (subtypes, rows) in additions {
        let links: Vec<Command> = rec_ids(subtypes)?.into_iter().map(Command::link).collect();
        followers
            .browse(rows)?
            .write(tx, RawVals::new().set_commands("subtype_ids", links))
            .await?;
    }
    Ok(())
}
