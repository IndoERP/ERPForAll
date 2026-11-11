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
