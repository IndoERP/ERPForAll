//! `mail.thread`: the abstract mixin that gives a model its message and follower storage.
//!
//! Writing `message_partner_ids` runs the inverse, which follows the added partners with
//! the default subtypes and unfollows the removed ones.
//! .

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::followers::{
    ExistingPolicy, MailFollowers, SubtypeChoice, insert_followers, subscriptions,
};
use crate::post::{PostRequest, Posted};

/// Most followers one [`MailThread`] `followers` page returns.
pub const MAX_FOLLOWER_PAGE: u32 = 100;
/// Most partner or subtype ids one follower call accepts.
pub const MAX_IDS_PER_CALL: usize = 1000;

/// The thread mixin: messages and followers of a record.
#[model(
    name = "mail.thread",
    description = "Email Thread",
    kind = "abstract",
    addon = "mail"
)]
pub struct MailThread {
    /// Messages of the record, user-specific notifications excluded.
    #[field(
        one2many,
        comodel = "mail.message",
        inverse_name = "res_id",
        domain = [("message_type", "!=", "user_notification")],
        string = "Messages"
    )]
    pub message_ids: Vec<RecId>,
    /// Follower rows of the record.
    #[field(
        one2many,
        comodel = "mail.followers",
        inverse_name = "res_id",
        groups = "base.group_user",
        string = "Followers"
    )]
    pub message_follower_ids: X2many<MailFollowers>,
    /// Partners following the record.
    #[field(
        many2many,
        comodel = "res.partner",
        compute = "_compute_message_partner_ids",
        inverse = "_inverse_message_partner_ids",
        compute_sudo = true,
        string = "Followers (Partners)"
    )]
    pub message_partner_ids: Vec<RecId>,
}

#[methods(MailThread, addon = "mail")]
impl MailThread {
    /// The partners of the record's follower rows.
    #[compute(depends = ["message_follower_ids", "message_follower_ids.partner_id"])]
    fn _compute_message_partner_ids(batch: &mut Batch<'_, MailThread>) -> Result<()> {
        for index in batch.indices() {
            let followers = batch.get(index, MailThread::MESSAGE_FOLLOWER_IDS)?;
            let mut partners = Vec::with_capacity(followers.len());
            for (follower, partner) in followers
                .ids()
                .iter()
                .zip(batch.values_of(MailFollowers::PARTNER_ID, followers.ids())?)
            {
                partners.push(partner.ok_or_else(|| {
                    Error::invariant(format!("mail.followers {follower:?} has no partner"))
                })?);
            }
            batch.set(index, MailThread::MESSAGE_PARTNER_IDS, partners)?;
        }
        Ok(())
    }

    /// Writing `message_partner_ids` makes the record's followers exactly those partners
    /// : the added partners follow with the default subtypes, the removed ones are
    /// unfollowed, both through `follow` and `unfollow` and their access rules. Only internal users
    /// may write the field. The new values are read first, because following invalidates them.
    ///
    /// # Errors
    /// `Access` for a caller who is not an internal user, and the errors of `follow` and
    /// `unfollow`.
    #[method]
    async fn _inverse_message_partner_ids(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        if records.is_empty() {
            return Ok(());
        }
        let env = records.env().clone();
        if !env.su() && !tx.has_group(&env, "base.group_user").await? {
            return Err(Error::access("Cannot write on message partners"));
        }
        let mut wanted = Vec::with_capacity(records.len());
        for record in records.iter() {
            match record.cell(tx, "message_partner_ids").await? {
                Cell::Ids(ids) => {
                    wanted.push(ids.iter().map(|id| id.get()).collect::<BTreeSet<_>>())
                }
                other => {
                    return Err(Error::invariant(format!(
                        "message_partner_ids is not an id list: {other:?}"
                    )));
                }
            }
        }
        let ids = records.ids();
        let followers = env.records("mail.followers")?.sudo(true);
        let domain = Domain::condition("res_model", Operator::Eq, records.model_name())?
            & Domain::condition("res_id", Operator::In, ids.clone())?;
        let rows = followers.search(tx, &domain).await?;
        let (row_res, row_partners) = (
            rows.cells(tx, "res_id").await?,
            rows.cells(tx, "partner_id").await?,
        );
        let mut previous: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
        for (res, partner) in row_res.iter().zip(&row_partners) {
            let (Cell::Id(res), Cell::Id(partner)) = (res, partner) else {
                return Err(Error::invariant(format!(
                    "unexpected follower row ({res:?}, {partner:?})"
                )));
            };
            previous.entry(*res).or_default().insert(*partner);
        }
        // unfollowing deletes rows and invalidates the caches, so it comes last
        let mut removals = Vec::new();
        for (id, wanted) in ids.iter().zip(wanted) {
            let before = previous.remove(id).unwrap_or_default();
            let record = records.browse([*id])?;
            let added: Vec<i64> = wanted.difference(&before).copied().collect();
            if added.len() > MAX_IDS_PER_CALL {
                return Err(Error::validation(format!(
                    "message_partner_ids adds {} partners to one record; at most {MAX_IDS_PER_CALL} \
                     can be added per write.",
                    added.len()
                )));
            }
            if !added.is_empty() {
                record
                    .call::<_, ()>(tx, "follow", (added, None::<Vec<i64>>))
                    .await?;
            }
            let removed: Vec<i64> = before.difference(&wanted).copied().collect();
            if removed.len() > MAX_IDS_PER_CALL {
                return Err(Error::validation(format!(
                    "message_partner_ids removes {} partners from one record; at most \
                     {MAX_IDS_PER_CALL} can be removed per write.",
                    removed.len()
                )));
            }
            if !removed.is_empty() {
                removals.push((record, removed));
            }
        }
        for (record, removed) in removals {
            record.call::<_, ()>(tx, "unfollow", (removed,)).await?;
        }
        Ok(())
    }

    /// Needs the access that
    /// `_mail_post_access` names (write by default, read on `res.partner`). The message is created
    /// with `subtype` (`mail.mt_note` by default), the parent the flat-thread rule finds, the
    /// current user's partner as author and a fresh Message-Id; it reaches the active partners that
    /// follow the record with the subtype (internal subtypes: internal partners only) and the active
    /// explicit and mentioned partners, never the author. A comment under `mail.mt_comment` makes an
    /// active internal author follow the record with the default subtypes. A `client_uid` seen before
    /// returns the stored message (`replayed`) when the same user posted it on the same record.
    ///
    /// # Errors
    /// `Validation` for anything but one saved record, a `user_notification` or unknown message
    /// type, an HTML body, a body over the limits, bad mentions or ids, an unknown subtype or
    /// partner, and a `client_uid` used for another post (also when another transaction posted it
    /// at the same moment, through the unique key); `Access` without the post access; `Invariant`
    /// when the user has no `res.users` row or no partner (a message needs an author).
    #[method]
    async fn message_post(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        request: PostRequest,
    ) -> Result<Posted> {
        crate::post::message_post(tx, this.as_records(), request).await
    }

    /// Models override it, as `res.partner` does (read).
    #[method]
    async fn _mail_post_access(
        _this: &RecordSet<MailThread>,
        _tx: &mut Transaction,
    ) -> Result<Operation> {
        Ok(Operation::Write)
    }

    /// Models override it.
    #[method]
    async fn _mail_flat_thread(
        _this: &RecordSet<MailThread>,
        _tx: &mut Transaction,
    ) -> Result<bool> {
        Ok(true)
    }

    #[method]
    async fn _message_post_after_hook(
        _this: &RecordSet<MailThread>,
        _tx: &mut Transaction,
        _message_id: i64,
    ) -> Result<()> {
        Ok(())
    }

    /// Returns the message ids in record order.
    ///
    /// # Errors
    /// See [`crate::post`]: `Validation` for an unsaved or repeated record id, a body of another
    /// record, too many records or a body over the limit; `Invariant` for a user without a partner.
    #[method]
    async fn _message_log_batch(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        bodies: BTreeMap<i64, String>,
    ) -> Result<Vec<i64>> {
        crate::post::message_log_batch(tx, this.as_records(), bodies).await
    }

    /// Deleting thread records deletes their messages, followers and scheduled messages
    /// : all point at the record through `model`/`res_id`, which no
    /// foreign key covers. The records go first, so a caller who may not delete them deletes
    /// nothing else either.
    #[method(override)]
    async fn unlink(this: &RecordSet<MailThread>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let records = this.as_records();
        if records.is_empty() {
            return sup.call(tx, records, ()).await;
        }
        let model = records.model_name();
        let ids = records.ids();
        sup.call(tx, records, ()).await?;
        let env = records.env();
        let messages = env.records("mail.message")?.sudo(true);
        let of_records = Domain::condition("model", Operator::Eq, model)?
            & Domain::condition("res_id", Operator::In, ids.clone())?;
        messages.search(tx, &of_records).await?.unlink(tx).await?;
        let followers = env.records("mail.followers")?.sudo(true);
        let of_records = Domain::condition("res_model", Operator::Eq, model)?
            & Domain::condition("res_id", Operator::In, ids.clone())?;
        followers.search(tx, &of_records).await?.unlink(tx).await?;
        // scheduled messages point at the record the same way: their delivery jobs go first, in
        // one set-based cancel, then the rows
        crate::scheduled::cancel_for_records(tx, env, model, &ids).await?;
        let scheduled = env.records("mail.scheduled.message")?.sudo(true);
        let of_records = Domain::condition("model", Operator::Eq, model)?
            & Domain::condition("res_id", Operator::In, ids)?;
        scheduled.search(tx, &of_records).await?.unlink(tx).await
    }

    /// The followers of one record, oldest first: at most `limit` (1 to [`MAX_FOLLOWER_PAGE`])
    /// follower records after the follower id `after` (a keyset page, §15.2), in the caller's
    /// environment. Needs read access to the record.
    ///
    /// # Errors
    /// `Validation` for a recordset of more than one record, a limit out of range or a
    /// non-positive `after`; `Access` when the caller may not read the record.
    #[method]
    async fn followers(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Records> {
        let record = this.as_records().ensure_one()?;
        if limit == 0 || limit > MAX_FOLLOWER_PAGE {
            return Err(Error::validation(format!(
                "A follower page holds 1 to {MAX_FOLLOWER_PAGE} followers, not {limit}."
            )));
        }
        let id = single_id(record)?;
        record.check_access(tx, Operation::Read).await?;
        let mut domain = Domain::condition("res_model", Operator::Eq, record.model_name())?
            & Domain::condition("res_id", Operator::Eq, id)?;
        if let Some(after) = after {
            if after <= 0 {
                return Err(Error::validation(format!(
                    "A follower page starts after a follower id, not {after}."
                )));
            }
            domain = domain & Domain::condition("id", Operator::Gt, after)?;
        }
        let options = SearchOptions {
            limit: Some(u64::from(limit)),
            order: Some("id".to_owned()),
            ..SearchOptions::default()
        };
        let followers = record.env().records("mail.followers")?;
        // found with privileges (the record's read access was checked), handed back in the
        // caller's environment: a superuser recordset never leaves this method
        let found = followers
            .sudo(true)
            .search_with(tx, &domain, &options)
            .await?;
        followers.browse(found.ids())
    }

    /// With `subtype_ids` every listed partner
    /// ends up with exactly those subtypes, existing followers included.
    ///
    /// Following another partner needs write access to the records, and only active partners are
    /// followed (inactive ones are dropped).
    /// The
    /// subtype ids must be existing subtypes with no model or the records' model.
    ///
    /// # Errors
    /// `Validation` for a non-positive id, more than [`MAX_IDS_PER_CALL`] ids or an unknown or
    /// foreign subtype id; `Access` as above; a follower that another transaction created at the
    /// same moment fails on the unique key as `Validation`.
    #[method]
    async fn follow(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_ids: Vec<i64>,
        subtype_ids: Option<Vec<i64>>,
    ) -> Result<()> {
        let records = this.as_records();
        let mut partner_ids = checked_ids("partner", partner_ids)?;
        let explicit = checked_ids("subtype", subtype_ids.unwrap_or_default())?;
        if records.is_empty() || partner_ids.is_empty() {
            return Ok(());
        }
        let env = records.env().clone();
        let found = subscriptions(tx, records.model_name(), &records.ids(), &[], env.uid()).await?;
        let only_self =
            matches!(partner_ids.as_slice(), [only] if found.user_partner == Some(*only));
        if only_self {
            records.check_access(tx, Operation::Read).await?;
            if !env.su() && tx.has_group(&env, "base.group_public").await? {
                return Err(Error::access("Public users cannot follow records."));
            }
        } else {
            records.check_access(tx, Operation::Write).await?;
            let active = Domain::condition("id", Operator::In, partner_ids)?
                & Domain::condition("active", Operator::Eq, true)?;
            partner_ids = env
                .records("res.partner")?
                .sudo(true)
                .search(tx, &active)
                .await?
                .ids();
        }
        if explicit.is_empty() {
            insert_followers(
                tx,
                records,
                &partner_ids,
                &SubtypeChoice::Defaults,
                ExistingPolicy::Skip,
            )
            .await
        } else {
            check_subtypes(tx, &env, records.model_name(), &explicit).await?;
            insert_followers(
                tx,
                records,
                &partner_ids,
                &SubtypeChoice::Explicit(explicit),
                ExistingPolicy::Replace,
            )
            .await
        }
    }

    /// `update` adds the missing subtypes to existing followers and
    /// never removes one. Private (not callable by RPC) and without access checks: the caller has
    /// checked access.
    ///
    /// # Errors
    /// `Validation` for an unknown policy, a non-positive id, too many ids or an unknown or foreign
    /// subtype id.
    #[method]
    async fn _insert_followers(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_ids: Vec<i64>,
        subtype_ids: Option<Vec<i64>>,
        policy: String,
    ) -> Result<()> {
        let policy = match policy.as_str() {
            "skip" => ExistingPolicy::Skip,
            "replace" => ExistingPolicy::Replace,
            "update" => ExistingPolicy::Update,
            other => {
                return Err(Error::validation(format!(
                    "The existing-follower policy is skip, replace or update, not '{other}'."
                )));
            }
        };
        let records = this.as_records();
        let partner_ids = checked_ids("partner", partner_ids)?;
        let explicit = checked_ids("subtype", subtype_ids.unwrap_or_default())?;
        let choice = if explicit.is_empty() {
            SubtypeChoice::Defaults
        } else {
            check_subtypes(tx, records.env(), records.model_name(), &explicit).await?;
            SubtypeChoice::Explicit(explicit)
        };
        insert_followers(tx, records, &partner_ids, &choice, policy).await
    }

    /// Removing another partner needs write access to the records; a user removing only their own
    /// partner needs none when internal and read access otherwise.
    ///
    /// # Errors
    /// `Validation` for a non-positive id or more than [`MAX_IDS_PER_CALL`] ids; `Access` as
    /// above.
    #[method]
    async fn unfollow(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_ids: Vec<i64>,
    ) -> Result<()> {
        let records = this.as_records();
        let partner_ids = checked_ids("partner", partner_ids)?;
        if records.is_empty() || partner_ids.is_empty() {
            return Ok(());
        }
        let env = records.env().clone();
        let found = subscriptions(
            tx,
            records.model_name(),
            &records.ids(),
            &partner_ids,
            env.uid(),
        )
        .await?;
        let only_self =
            matches!(partner_ids.as_slice(), [only] if found.user_partner == Some(*only));
        if !only_self {
            records.check_access(tx, Operation::Write).await?;
        } else if !env.su() && !tx.has_group(&env, "base.group_user").await? {
            records.check_access(tx, Operation::Read).await?;
        }
        env.records("mail.followers")?
            .sudo(true)
            .browse(found.followers)?
            .unlink(tx)
            .await
    }

    /// Changing another partner's
    /// subtypes needs write access to the record, one's own read access.
    /// The write is pending
    /// until the next flush.
    ///
    /// # Errors
    /// `Validation` for a recordset of more than one record, non-positive ids, too many, an empty
    /// list ("Choose at least one subtype, or unfollow.") or an unknown or foreign subtype id;
    /// `Access` as above; `Missing` when the partner does not follow the record.
    #[method]
    async fn set_subtypes(
        this: &RecordSet<MailThread>,
        tx: &mut Transaction,
        partner_id: i64,
        subtype_ids: Vec<i64>,
    ) -> Result<()> {
        let record = this.as_records().ensure_one()?;
        let partner = checked_ids("partner", vec![partner_id])?;
        let subtype_ids = checked_ids("subtype", subtype_ids)?;
        if subtype_ids.is_empty() {
            return Err(Error::validation(
                "Choose at least one subtype, or unfollow.",
            ));
        }
        let id = single_id(record)?;
        let env = record.env().clone();
        let found = subscriptions(tx, record.model_name(), &[id], &partner, env.uid()).await?;
        let operation = if found.user_partner == Some(partner_id) {
            Operation::Read
        } else {
            Operation::Write
        };
        record.check_access(tx, operation).await?;
        let [follower] = found.followers.as_slice() else {
            return Err(Error::missing(format!(
                "Partner {partner_id} does not follow {} {id}.",
                record.model_name()
            )));
        };
        check_subtypes(tx, &env, record.model_name(), &subtype_ids).await?;
        let subtypes = subtype_ids
            .into_iter()
            .map(|id| RecId::from_real(id).ok_or_else(|| invalid_id("subtype", id)))
            .collect::<Result<Vec<_>>>()?;
        env.records("mail.followers")?
            .sudo(true)
            .browse([*follower])?
            .write(
                tx,
                RawVals::new().set_commands("subtype_ids", [Command::set(subtypes)]),
            )
            .await
    }
}

/// The id of a recordset `ensure_one` accepted.
fn single_id(record: &Records) -> Result<i64> {
    record
        .ids()
        .first()
        .copied()
        .ok_or_else(|| Error::invariant("a single-record call has no record id"))
}

fn invalid_id(what: &str, id: i64) -> Error {
    Error::validation(format!("A {what} id must be a positive integer, not {id}."))
}

/// `ids` deduplicated in ascending order; every id positive, at most [`MAX_IDS_PER_CALL`].
pub(crate) fn checked_ids(what: &str, ids: Vec<i64>) -> Result<Vec<i64>> {
    if ids.len() > MAX_IDS_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_IDS_PER_CALL} {what} ids are accepted per call, not {}.",
            ids.len()
        )));
    }
    let mut set = BTreeSet::new();
    for id in ids {
        if id <= 0 {
            return Err(invalid_id(what, id));
        }
        set.insert(id);
    }
    Ok(set.into_iter().collect())
}

/// Every id of `ids` must be an existing `mail.message.subtype` with no model or `model`, so a
/// wrong id fails here with a message and not on the foreign key at flush. The subtypes are found
/// with privileges: only their existence is learned, and the caller has been checked on the
/// record.
///
/// # Errors
/// `Validation` naming the ids that are unknown or belong to another model.
pub(crate) async fn check_subtypes(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: &[i64],
) -> Result<()> {
    let domain = Domain::condition("id", Operator::In, ids.to_vec())?
        & (Domain::condition("res_model", Operator::Eq, model)?
            | Domain::condition("res_model", Operator::Eq, false)?);
    let found: BTreeSet<i64> = env
        .records("mail.message.subtype")?
        .sudo(true)
        .search(tx, &domain)
        .await?
        .ids()
        .into_iter()
        .collect();
    let bad: Vec<String> = ids
        .iter()
        .filter(|id| !found.contains(id))
        .map(ToString::to_string)
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(Error::validation(format!(
        "Unknown subtype ids for {model}: {}. A subtype must exist and have no model or the \
         model of the record.",
        bad.join(", ")
    )))
}
