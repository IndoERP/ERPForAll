//! Base addon migrations.

use erp_sdk::migrations::SemverVersion;
use erp_sdk::{
    Cell, Context, Domain, Error, ErrorKind, Operator, RawVals, Result, SearchOptions, Transaction,
    Value,
};
use std::sync::Arc;

/// Protect existing tenant catalog values before upgrade data files can update them.
/// Schema/data/company migration failure rolls this flag transition back atomically.
#[erp_sdk::migration(addon = "base", version = "1.1", stage = pre)]
pub async fn protect_base_1_1_catalogs(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    tx.execute(
        "UPDATE ir_model_data SET noupdate = true WHERE module = 'base' AND model IN ('res.currency', 'res.lang')",
        &[],
    )
    .await?;
    Ok(())
}

/// Link existing companies after the new schema and default identities are loaded.
#[erp_sdk::migration(addon = "base", version = "1.1", stage = post)]
pub async fn migrate_base_1_1(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    // 2. Link main_partner to main_company if partner_id is NULL
    tx.execute(
        "UPDATE res_company c \
         SET partner_id = p_data.res_id \
         FROM ir_model_data p_data, ir_model_data c_data \
         WHERE p_data.module = 'base' \
           AND p_data.name = 'main_partner' \
           AND p_data.model = 'res.partner' \
           AND p_data.res_id IS NOT NULL \
           AND c_data.module = 'base' \
           AND c_data.name = 'main_company' \
           AND c_data.model = 'res.company' \
           AND c_data.res_id = c.id \
           AND c.partner_id IS NULL",
        &[],
    )
    .await?;

    // 3. Lock only companies still missing a representative. Creating a row with SQL
    // bypasses partner defaults and stored computes once the full partner model is installed.
    // Normal create/write also preserve addon dispatch and the transaction's dependency cache.
    let companies = tx
        .query(
            "SELECT id, name FROM res_company WHERE partner_id IS NULL ORDER BY id FOR UPDATE",
            &[],
        )
        .await?;
    for company in companies {
        let company_id: i64 = company
            .try_get(0)
            .map_err(|error| Error::db(format!("company partner migration identity: {error}")))?;
        let name: String = company
            .try_get(1)
            .map_err(|error| Error::db(format!("company partner migration name: {error}")))?;
        let partners = tx.env().records("res.partner")?;
        let partner = partners
            .create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("company_id", Cell::Id(company_id))
                        .set_cell("active", Cell::Bool(true)),
                ],
            )
            .await?;
        let ids = partner.ids();
        let [partner_id] = ids.as_slice() else {
            return Err(Error::programming(
                "company partner migration expected exactly one created contact",
            ));
        };
        let company = tx.env().records("res.company")?.browse([company_id])?;
        company
            .write(
                tx,
                RawVals::new().set_cell("partner_id", Cell::Id(*partner_id)),
            )
            .await?;
    }

    Ok(())
}

/// Initialize an existing category store before base 1.8 enables parent-store metadata.
///
/// This is a lifecycle migration, not a record mutation entry point. It runs with the
/// upgrade owner's authority inside the caller's lifecycle transaction, before schema
/// conversion of translated category names. Existing correct paths are left unchanged;
/// no names, colors, active flags, identities or partner links are rewritten.
///
/// A legacy release without a category table has nothing to migrate: schema synchronization
/// creates its empty store. An existing store is locked against concurrent writes, validated
/// completely, then filled set-wise. Root depth is one and the admission ceiling is 256.
///
/// # Errors
///
/// Returns `Validation` for a cycle/missing parent or depth above 256, before any path update.
/// Database/decoding failures propagate. A later lifecycle failure rolls these updates back
/// with the schema, data and module-version changes; this function never commits.
#[erp_sdk::migration(addon = "base", version = "1.8", stage = pre)]
pub async fn prepare_base_1_8_category_paths(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    let presence = tx
        .query(
            "SELECT to_regclass('public.res_partner_category') IS NOT NULL",
            &[],
        )
        .await?;
    let [presence] = presence.as_slice() else {
        return Err(Error::programming(
            "base 1.8 category migration expected one table-presence row",
        ));
    };
    let present: bool = presence.try_get(0).map_err(|error| {
        Error::db("base 1.8 category table-presence decode failed").with_source(error)
    })?;
    if !present {
        // An older release with no category model has no category records or paths to preserve.
        return Ok(());
    }

    tx.execute(
        "LOCK TABLE \"res_partner_category\" IN SHARE ROW EXCLUSIVE MODE",
        &[],
    )
    .await?;

    // The traversal includes depth 257 solely to detect the first refused level. Root-based
    // expansion cannot enter a cycle; every row in a valid single-parent forest is reached
    // exactly once. Unreached rows therefore identify a cycle or an orphan. Only one bounded
    // summary row crosses into Rust, and no DML is sent before both admissions succeed.
    let max_depth = crate::res_partner::MAX_CATEGORY_DEPTH;
    let summary = tx
        .query(
            &format!(
                "WITH RECURSIVE tree(id, depth) AS ( \
                 SELECT id, 1 FROM \"res_partner_category\" WHERE parent_id IS NULL \
                 UNION ALL \
                 SELECT child.id, tree.depth + 1 FROM \"res_partner_category\" child \
                 JOIN tree ON child.parent_id = tree.id WHERE tree.depth <= {max_depth} \
                 ) SELECT COALESCE(bool_or(depth > {max_depth}), false), count(*), \
                 (SELECT count(*) FROM \"res_partner_category\") FROM tree"
            ),
            &[],
        )
        .await?;
    let [summary] = summary.as_slice() else {
        return Err(Error::programming(
            "base 1.8 category migration expected one hierarchy-summary row",
        ));
    };
    let decode =
        |error| Error::db("base 1.8 category hierarchy-summary decode failed").with_source(error);
    let too_deep: bool = summary.try_get(0).map_err(decode)?;
    let reached: i64 = summary.try_get(1).map_err(decode)?;
    let total: i64 = summary.try_get(2).map_err(decode)?;
    if too_deep {
        return Err(Error::validation(
            "base 1.8 category migration refused: category hierarchy exceeds depth 256",
        ));
    }
    if reached != total {
        return Err(Error::validation(
            "base 1.8 category migration refused: category hierarchy contains a cycle or a missing parent",
        ));
    }

    // Depth bounds also bound each copied path: an int8 id plus '/' is at most 21 bytes.
    // The table lock keeps this second traversal over the already admitted forest stable.
    tx.execute(
        &format!(
            "WITH RECURSIVE tree(id, path, depth) AS ( \
             SELECT id, id::text || '/', 1 FROM \"res_partner_category\" WHERE parent_id IS NULL \
             UNION ALL \
             SELECT child.id, tree.path || child.id::text || '/', tree.depth + 1 \
             FROM \"res_partner_category\" child JOIN tree ON child.parent_id = tree.id \
             WHERE tree.depth < {max_depth} \
             ) UPDATE \"res_partner_category\" category SET parent_path = tree.path \
             FROM tree WHERE category.id = tree.id AND category.parent_path IS DISTINCT FROM tree.path"
        ),
        &[],
    )
    .await?;
    Ok(())
}

fn partner_label_migration_error(id: i64, error: Error) -> Error {
    // Only the known, contact-free size diagnostic receives a record identity.
    // An unrelated native or database error retains its kind and original cause.
    const OUTPUT_LIMIT: &str = "Contact label output exceeds the 1048576-byte limit";
    if error.kind() == ErrorKind::Validation && error.message() == OUTPUT_LIMIT {
        Error::validation(format!(
            "base 1.2 label recomputation failed for res.partner {id}: {OUTPUT_LIMIT}"
        ))
    } else {
        error
    }
}

/// Refresh stored contact labels after the complete-name fallback correction.
///
/// Includes archived contacts, retains identities and source fields, and runs in
/// the enclosing lifecycle transaction. Native helper and write overrides remain
/// active. A failed helper, invalid output or later lifecycle failure aborts the
/// entire upgrade; the migration never commits an individual chunk.
#[erp_sdk::migration(addon = "base", version = "1.2", stage = post)]
async fn recompute_partner_labels_base_1_2(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    // Keyset windows bound prefetched inputs and operation-local buffers.
    // Flushing and invalidating after each window releases the transaction's
    // value cache instead of retaining every contact in memory.
    const CHUNK_SIZE: u64 = 256;
    let source_env = tx.env().with_context_replaced(Context::new());
    let scan = source_env
        .records("res.partner")?
        .with_context([("active_test", Value::Bool(false))]);
    let mut after = 0_i64;
    loop {
        let chunk = scan
            .search_with(
                tx,
                &Domain::condition("id", Operator::Gt, after)?,
                &SearchOptions::new()
                    .order("id")
                    .limit(CHUNK_SIZE)
                    .fetch_fields([
                        "name",
                        "type",
                        "parent_id",
                        "is_company",
                        "commercial_company_name",
                        "complete_name",
                    ]),
            )
            .await?;
        if chunk.is_empty() {
            break;
        }

        // Stored labels use source selection labels and no caller display flags.
        // The scan's active_test=false is solely an enumeration concern.
        let records = chunk.with_env(source_env.clone());
        let stored = records.cells(tx, "complete_name").await?;
        if stored.len() != records.len() {
            return Err(Error::programming(format!(
                "res.partner.complete_name: expected {} stored values, got {}",
                records.len(),
                stored.len()
            )));
        }

        let ids = records.ids();
        for (id, old) in ids.iter().copied().zip(stored) {
            // Singleton dispatch identifies even a helper-raised size refusal
            // without retrying a failed native call or its transaction. The
            // helper's ordinary override chain and the chunk's prefetch remain.
            let record = records.browse([id])?;
            let labels = record
                .call::<(), Vec<Arc<str>>>(tx, "_get_complete_name", ())
                .await
                .map_err(|error| partner_label_migration_error(id, error))?;
            let [label] = labels.as_slice() else {
                return Err(Error::programming(format!(
                    "res.partner._get_complete_name: expected 1 labels, got {}",
                    labels.len()
                )));
            };
            crate::partner_labels::check_label(label)
                .map_err(|error| partner_label_migration_error(id, error))?;
            let unchanged = match old {
                Cell::Str(old) => old.as_ref() == label.as_ref(),
                Cell::Null => false,
                _ => {
                    return Err(Error::programming(
                        "res.partner.complete_name: expected a string or null",
                    ));
                }
            };
            if !unchanged {
                record
                    .write(
                        tx,
                        RawVals::new().set_cell("complete_name", Cell::Str(label.clone())),
                    )
                    .await?;
            }
        }
        let Some(next) = ids.last().copied().filter(|next| *next > after) else {
            return Err(Error::programming(
                "partner label migration did not advance its identity window",
            ));
        };
        after = next;
        tx.flush().await?;
        tx.invalidate_all().await?;
    }
    Ok(())
}

/// Admit the branch lifecycle without silently changing legacy financial configuration.
/// The enclosing module lifecycle transaction rolls back schema, data and version together.
#[erp_sdk::migration(addon = "base", version = "1.5", stage = post)]
async fn validate_company_branches_base_1_5(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    crate::company_lifecycle::fence(tx).await?;
    tx.flush().await?;
    // Exact materialized-path equality also rejects cycles: a cycle cannot have every path
    // equal to its parent's strictly longer path. Inspect only the first offending identity.
    let rows = tx.query(
        "SELECT c.id FROM res_company c LEFT JOIN res_company p ON p.id = c.parent_id \
         WHERE c.currency_id IS NULL \
         OR (c.parent_id IS NOT NULL AND (p.id IS NULL OR c.currency_id IS DISTINCT FROM p.currency_id)) \
         OR c.parent_path IS DISTINCT FROM (COALESCE(p.parent_path, '') || c.id::text || '/') \
         OR length(c.parent_path) - length(replace(c.parent_path, '/', '')) > 256 \
         ORDER BY c.id LIMIT 1", &[]).await?;
    if let Some(row) = rows.first() {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("base 1.5 company validation identity: {e}")))?;
        return Err(Error::validation(format!(
            "base 1.5 company hierarchy/currency validation failed for company {id}; repair parent paths and branch currencies before upgrading."
        )));
    }
    // A genuine old schema receives the active=true fast column default. NULL backfill also
    // handles an interrupted/manual legacy preparation; existing false archive state is retained.
    tx.execute(
        "UPDATE res_company SET active = true WHERE active IS NULL",
        &[],
    )
    .await?;
    tx.invalidate_model("res.company", Some(&["active"]))
        .await?;
    let invalid = tx.query("SELECT 1 FROM res_users u JOIN res_company c ON c.id = u.company_id WHERE u.active IS TRUE AND c.active IS NOT TRUE LIMIT 1", &[]).await?;
    if !invalid.is_empty() {
        return Err(Error::validation(
            "base 1.5 company validation failed: an active user has an archived default company; repair the user or company before upgrading.",
        ));
    }
    Ok(())
}

/// Give every existing exchange rate the `spot` type before the schema step makes
/// `res.currency.rate.rate_type_id` required (step 1, foundation L-3).
///
/// Schema synchronization refuses a new required column without a static default on a table that
/// has rows, and a type's id is not static. So on an older database this step runs first: it
/// creates the type table, inserts the `spot` row with the external id `base.rate_type_spot`
/// (`noupdate`, so the data file leaves it alone), adds the column and points every existing rate
/// at `spot`. Nothing is guessed from a label: every old rate is spot, as before. Fresh installs
/// and databases that already have the column do nothing here.
#[erp_sdk::migration(addon = "base", version = "1.6", stage = pre)]
async fn prepare_rate_types_base_1_6(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    prepare_rate_types(tx).await
}

/// The body of [`prepare_rate_types_base_1_6`], callable on its own (the upgrade test runs it
/// against a database whose schema it first sets back to the old shape).
pub async fn prepare_rate_types(tx: &mut Transaction) -> Result<()> {
    let state = tx
        .query(
            "SELECT to_regclass('res_currency_rate') IS NOT NULL, \
             EXISTS (SELECT 1 FROM information_schema.columns \
               WHERE table_schema = current_schema() AND table_name = 'res_currency_rate' \
               AND column_name = 'rate_type_id')",
            &[],
        )
        .await?;
    let (has_table, has_column): (bool, bool) = match state.first() {
        Some(row) => (
            row.try_get(0)
                .map_err(|e| Error::db(format!("base 1.6 rate table probe: {e}")))?,
            row.try_get(1)
                .map_err(|e| Error::db(format!("base 1.6 rate column probe: {e}")))?,
        ),
        None => return Err(Error::db("base 1.6 rate table probe returned no row")),
    };
    if !has_table || has_column {
        return Ok(());
    }
    let model = tx
        .registry()
        .model("res.currency.rate.type")
        .map(|m| m.id)
        .ok_or_else(|| Error::programming("the registry has no res.currency.rate.type model"))?;
    tx.init_models(&[model]).await?;
    let records = tx.env().records("res.currency.rate.type")?;
    let spot = records
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("code", Cell::Str(crate::SPOT_RATE_TYPE.into()))
                    .set_cell("name", Cell::Str("Spot".into()))
                    .set_cell("date_rule", Cell::Str("strictly_before".into()))
                    .set_cell("later_fallback", Cell::Bool(true))
                    .set_cell("sequence", Cell::Int(10)),
            ],
        )
        .await?
        .rec_ids()[0]
        .get();
    tx.flush().await?;
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ('base', 'rate_type_spot', 'res.currency.rate.type', $1, true)",
        &[&spot],
    )
    .await?;
    tx.execute(
        "ALTER TABLE res_currency_rate ADD COLUMN rate_type_id int8",
        &[],
    )
    .await?;
    tx.execute("UPDATE res_currency_rate SET rate_type_id = $1", &[&spot])
        .await?;
    Ok(())
}

/// Recompute `res.users.share` and `res.partner.partner_share` for every user and contact after
/// base 1.9 made them the escapes of the company restrictions of users and contacts.
/// Before 1.9 `partner_share` was a placeholder (true unless the contact's id was
/// the superuser id), so every stored value is rewritten with the rule.
///
/// No pre stage: both fields are required with the static default `true`, and the schema step
/// that makes a column required fills its NULLs with that default before it sets NOT NULL (on a
/// large table as a post-commit step; this recompute also rewrites a NULL it still finds). Fresh
/// installs compute both fields on create and do not run this.
///
/// # Errors
///
/// `Programming` when the registry has no `ir.model.data` model (the internal groups are named by
/// external id); `Database` for a failed statement. A later lifecycle failure rolls the rewrite
/// back with the schema, data and module-version changes.
#[erp_sdk::migration(addon = "base", version = "1.9", stage = post)]
async fn recompute_shares_base_1_9(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    recompute_shares(tx).await
}

/// The body of [`recompute_shares_base_1_9`], callable on its own (the upgrade test runs it
/// against a database whose schema it first sets back to the old shape): the set-based recompute
/// of every user and every contact ([`crate::group_graph::recompute_user_share`]).
///
/// # Errors
///
/// As [`crate::group_graph::recompute_user_share`].
pub async fn recompute_shares(tx: &mut Transaction) -> Result<()> {
    crate::group_graph::recompute_user_share(tx, None).await
}

/// Give every existing user a default company before base 1.10 enforces the rule that an active
/// user's default company is one of its allowed companies.
///
/// A post step: nothing becomes NOT NULL, and by now `base.main_company` and the new registry
/// exist. Fresh installs create no users before the first administrator and do not run this.
#[erp_sdk::migration(addon = "base", version = "1.10", stage = post)]
async fn assign_user_companies_base_1_10(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    assign_user_companies(tx).await
}

/// How many user ids a refusal lists before it says how many more there are.
const REFUSAL_IDS_SHOWN: usize = 50;

fn collect_ids<E: std::fmt::Display>(
    ids: impl Iterator<Item = std::result::Result<i64, E>>,
    what: &str,
) -> Result<Vec<i64>> {
    ids.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::db(format!("{what}: {e}")))
}

/// The body of [`assign_user_companies_base_1_10`], callable on its own (the upgrade test runs it
/// against rows it inserts in the old shape). The superuser is skipped: it keeps no company.
/// Set-based, under the company lifecycle fence and a table lock, and without
/// reading `Main Company` into any rule: a grant the operator never chose would widen access.
///
/// 1. **R1.** A user whose `company_id` is not among its `company_ids` is never given the
///    membership: a migration does not widen access (lead decision 2026-11-12, decision
///    7). **R1a:** when the user still has an active allowed company, `company_id` becomes the
///    lowest-id one among them and no row is inserted. The principal resolver builds the company
///    set from the relation rows alone and refuses a default company outside it (`erp-security`
///    `resolver.rs` `SubjectRow::resolution`, `erp-orm` `Resolution::with_default_order`), so such
///    a user could not sign in before; now it can, in a company an administrator allowed. **R1b:**
///    a user with no active allowed company at all (no row, or only archived companies) keeps the
///    earlier handling: its default company is added to its allowed ones, as nothing else can be
///    chosen without guessing. The warning logs name the user ids.
/// 2. **R2.** A user without `company_id` and with `company_ids` takes the lowest-id active
///    company among them.
/// 3. **R3a.** A user with neither, in a database with exactly one company that is active, takes
///    that company for both fields (active and inactive users alike).
/// 4. **R3b.** Any active user that still has no default company refuses the upgrade before a row
///    is written: there is no company that can be chosen without guessing. Inactive users are
///    never a reason to refuse (R1, R2 and R3a fill them like active ones); an inactive user that
///    stays without a default company is refused later, when someone reactivates it.
///
/// A database without any company has nothing to assign and is left alone (the default company
/// rule does not apply to it). Each rule that changed rows logs one warning on the target
/// `erp::migration` with the user ids.
///
/// # Errors
///
/// `Validation` (R3b, naming the user ids and the number of companies); `Database` for a failed
/// statement. A later lifecycle failure rolls the changes back with the schema, data and
/// module-version changes.
pub async fn assign_user_companies(tx: &mut Transaction) -> Result<()> {
    crate::company_lifecycle::fence(tx).await?;
    tx.execute(
        "LOCK TABLE res_users, res_company_users_rel IN SHARE ROW EXCLUSIVE MODE",
        &[],
    )
    .await?;
    tx.flush().await?;
    let superuser = erp_sdk::SUPERUSER_ID.get();

    let companies = tx
        .query(
            "SELECT count(*), count(*) FILTER (WHERE active IS TRUE), \
                    min(id::int8) FILTER (WHERE active IS TRUE) FROM res_company",
            &[],
        )
        .await?;
    let [companies] = companies.as_slice() else {
        return Err(Error::programming(
            "base 1.10 company count expected one summary row",
        ));
    };
    let decode = |e| Error::db(format!("base 1.10 company count decode: {e}"));
    let total: i64 = companies.try_get(0).map_err(decode)?;
    let active: i64 = companies.try_get(1).map_err(decode)?;
    let lowest_active: Option<i64> = companies.try_get(2).map_err(decode)?;
    if total == 0 {
        return Ok(());
    }
    // the only company, when there is exactly one and it is active
    let only = if total == 1 && active == 1 {
        lowest_active
    } else {
        None
    };

    // R3b first, read-only: nothing is written when the upgrade refuses
    let unassignable = tx
        .query(
            "SELECT u.id::int8 FROM res_users u \
              WHERE u.id <> $1 AND u.active IS TRUE AND u.company_id IS NULL \
                AND NOT $2::bool \
                AND NOT EXISTS ( \
                      SELECT 1 FROM res_company_users_rel r \
                        JOIN res_company c ON c.id = r.cid \
                       WHERE r.user_id = u.id AND c.active IS TRUE) \
              ORDER BY u.id",
            &[&superuser, &only.is_some()],
        )
        .await?;
    if !unassignable.is_empty() {
        let ids = collect_ids(
            unassignable.iter().map(|row| row.try_get(0)),
            "base 1.10 refusal ids",
        )?;
        let mut listed: Vec<String> = ids
            .iter()
            .take(REFUSAL_IDS_SHOWN)
            .map(i64::to_string)
            .collect();
        if ids.len() > REFUSAL_IDS_SHOWN {
            listed.push(format!("and {} more", ids.len() - REFUSAL_IDS_SHOWN));
        }
        return Err(Error::validation(format!(
            "base 1.10 refused: active users {} have no default company and none can be chosen \
             ({total} companies). Set each user's default and allowed companies in Settings > \
             Users with the current release, then upgrade again.",
            listed.join(", ")
        )));
    }

    // R1a: a default company outside the user's allowed companies is never added to them (a
    // migration does not widen access); the user takes the lowest-id active
    // allowed company instead
    let rows = tx
        .query(
            "UPDATE res_users u SET company_id = m.cid \
               FROM (SELECT r.user_id, min(r.cid) AS cid \
                       FROM res_company_users_rel r \
                       JOIN res_company c ON c.id = r.cid \
                      WHERE c.active IS TRUE GROUP BY r.user_id) m \
              WHERE u.id = m.user_id AND u.id <> $1 AND u.company_id IS NOT NULL \
                AND NOT EXISTS ( \
                      SELECT 1 FROM res_company_users_rel r \
                       WHERE r.user_id = u.id AND r.cid = u.company_id) \
             RETURNING u.id::int8",
            &[&superuser],
        )
        .await?;
    if !rows.is_empty() {
        let ids = collect_ids(rows.iter().map(|row| row.try_get(0)), "base 1.10 R1a ids")?;
        tracing::warn!(
            target: "erp::migration",
            user_ids = ?ids,
            "base 1.10: the default company of these users was not one of their allowed companies; they took their lowest-id active allowed company"
        );
    }

    // R1b: what is left has no active allowed company at all, so the default company is added to
    // the allowed ones as before
    let rows = tx
        .query(
            "INSERT INTO res_company_users_rel (user_id, cid) \
             SELECT u.id, u.company_id FROM res_users u \
              WHERE u.id <> $1 AND u.company_id IS NOT NULL \
                AND NOT EXISTS ( \
                      SELECT 1 FROM res_company_users_rel r \
                       WHERE r.user_id = u.id AND r.cid = u.company_id) \
              ORDER BY u.id \
             RETURNING user_id::int8",
            &[&superuser],
        )
        .await?;
    if !rows.is_empty() {
        let ids = collect_ids(rows.iter().map(|row| row.try_get(0)), "base 1.10 R1b ids")?;
        tracing::warn!(
            target: "erp::migration",
            user_ids = ?ids,
            "base 1.10: the default company of these users was added to their allowed companies"
        );
    }

    // R2
    let rows = tx
        .query(
            "UPDATE res_users u SET company_id = m.cid \
               FROM (SELECT r.user_id, min(r.cid) AS cid \
                       FROM res_company_users_rel r \
                       JOIN res_company c ON c.id = r.cid \
                      WHERE c.active IS TRUE GROUP BY r.user_id) m \
              WHERE u.id = m.user_id AND u.id <> $1 AND u.company_id IS NULL \
             RETURNING u.id::int8",
            &[&superuser],
        )
        .await?;
    if !rows.is_empty() {
        let ids = collect_ids(rows.iter().map(|row| row.try_get(0)), "base 1.10 R2 ids")?;
        tracing::warn!(
            target: "erp::migration",
            user_ids = ?ids,
            "base 1.10: these users had no default company and took their lowest-id active allowed company"
        );
    }

    // R3a
    if let Some(company) = only {
        let rows = tx
            .query(
                "UPDATE res_users SET company_id = $1 \
                  WHERE id <> $2 AND company_id IS NULL \
                 RETURNING id::int8",
                &[&company, &superuser],
            )
            .await?;
        if !rows.is_empty() {
            let ids = collect_ids(rows.iter().map(|row| row.try_get(0)), "base 1.10 R3a ids")?;
            tx.execute(
                "INSERT INTO res_company_users_rel (user_id, cid) \
                 SELECT u.id, $1 FROM res_users u \
                  WHERE u.id = ANY($2::int8[]) \
                    AND NOT EXISTS ( \
                          SELECT 1 FROM res_company_users_rel r \
                           WHERE r.user_id = u.id AND r.cid = $1)",
                &[&company, &ids],
            )
            .await?;
            tracing::warn!(
                target: "erp::migration",
                user_ids = ?ids,
                company,
                "base 1.10: these users had no company and took the only company of the database"
            );
        }
    }

    tx.invalidate_model("res.users", Some(&["company_id", "company_ids"]))
        .await?;
    Ok(())
}
