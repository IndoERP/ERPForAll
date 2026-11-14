// SPDX-License-Identifier: MIT OR Apache-2.0
//! Barcode claims and permanent scope fences. Values are exact stored strings.
//! A write to the common scope row makes a stale repeatable-read writer fail before it can
//! validate against an old snapshot. All supported holder mutations reconcile in their transaction.
use erp_sdk::prelude::*;
use std::collections::BTreeMap;

/// Holder projection: include archived rows and collapse only the exact single-variant mirror.
/// `keys` is [`Keys::sql`]: a bound text array of affected barcode keys, or every stored
/// barcode for the upgrade backfill. No caller selects SQL identifiers.
fn holders(keys: &str) -> String {
    format!(
        "SELECT 'product.product'::text AS model, p.id AS holder, t.company_id, p.barcode,
        p.id AS variant_id, NULL::bigint AS template_id, NULL::bigint AS packaging_id
    FROM product_product p JOIN product_template t ON t.id = p.product_tmpl_id
    WHERE p.barcode {keys} AND p.barcode <> ''
    UNION ALL
    SELECT 'product.template', t.id, t.company_id, t.barcode, NULL::bigint, t.id, NULL::bigint
    FROM product_template t WHERE t.barcode {keys} AND t.barcode <> ''
        AND NOT EXISTS (SELECT 1 FROM product_product p WHERE p.product_tmpl_id = t.id
            AND p.barcode = t.barcode AND NOT EXISTS (SELECT 1 FROM product_product other
                WHERE other.product_tmpl_id = t.id AND other.id <> p.id))
    UNION ALL
    SELECT 'product.uom', u.id, u.company_id, u.barcode, NULL::bigint, NULL::bigint, u.id
    FROM product_uom u WHERE u.barcode {keys} AND u.barcode <> ''"
    )
}

/// The barcode keys a holder query covers.
#[derive(Clone, Copy)]
enum Keys<'a> {
    /// The affected keys of one mutation, bound as `$1`.
    Listed(&'a [String]),
    /// Every stored barcode (upgrade backfill), with no parameter.
    All,
}

impl Keys<'_> {
    fn sql(self) -> &'static str {
        match self {
            Keys::Listed(_) => "= ANY($1)",
            Keys::All => "IS NOT NULL",
        }
    }
}

/// A template barcode equal to one of its own variants' barcodes while the template has more
/// than one variant (archived ones count): the mirror of a former single variant, which the
/// variant keeps. Alias `t` is `product_template`.
const STALE_MIRROR: &str = "t.barcode <> ''
    AND (SELECT count(*) FROM product_product p WHERE p.product_tmpl_id = t.id) > 1
    AND EXISTS (SELECT 1 FROM product_product p WHERE p.product_tmpl_id = t.id AND p.barcode = t.barcode)";

/// The assignment that ends a stale mirror, stamped like an ORM write (`$1` is the writer).
const CLEAR_MIRROR: &str =
    "barcode = NULL, write_uid = $1, write_date = (now() AT TIME ZONE 'UTC')";
/// The template fields [`CLEAR_MIRROR`] changes.
const CLEARED_FIELDS: [&str; 3] = ["barcode", "write_uid", "write_date"];

/// Templates among `families` whose barcode mirrors their only variant (archived ones count).
/// Read before a mutation that can add a variant to a family, for [`drop_stale_mirrors`].
pub(crate) async fn single_mirrors(tx: &mut Transaction, families: &[i64]) -> Result<Vec<i64>> {
    if families.is_empty() {
        return Ok(Vec::new());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT t.id FROM product_template t WHERE t.id = ANY($1) AND t.barcode <> ''
            AND (SELECT count(*) FROM product_product p WHERE p.product_tmpl_id = t.id) = 1
            AND EXISTS (SELECT 1 FROM product_product p WHERE p.product_tmpl_id = t.id
                AND p.barcode = t.barcode)",
            &[&families],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            row.try_get(0)
                .map_err(|e| Error::db("Barcode mirror identity decode failed.").with_source(e))
        })
        .collect()
}

/// Clear the template mirror of the `mirrored` families that now have more than one variant;
/// the variant keeps the barcode. The compute leaves a multi-variant
/// template's barcode alone, so without this the template and its own variant would collide.
pub(crate) async fn drop_stale_mirrors(tx: &mut Transaction, mirrored: &[i64]) -> Result<()> {
    if mirrored.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let uid = tx.env().uid();
    let cleared = tx
        .execute(
            &format!("UPDATE product_template t SET {CLEAR_MIRROR} WHERE t.id = ANY($2) AND {STALE_MIRROR}"),
            &[&uid, &mirrored],
        )
        .await?;
    if cleared > 0 {
        tx.invalidate_model("product.template", Some(&CLEARED_FIELDS))
            .await?;
    }
    Ok(())
}

pub(crate) fn refuse_bypass(env: &Env) -> Result<()> {
    if env.context().get("skip_barcode_uniqueness_check").is_some() {
        return Err(Error::validation(
            "skip_barcode_uniqueness_check is not supported: barcode uniqueness cannot be skipped.",
        ));
    }
    Ok(())
}

/// Lock old and proposed values before source writes, including defaults through final reconciliation.
pub(crate) async fn prepare(
    tx: &mut Transaction,
    env: &Env,
    templates: &[i64],
    packagings: &[i64],
    values: &[RawVals],
) -> Result<Vec<String>> {
    refuse_bypass(env)?;
    tx.flush().await?;
    let mut keys = keys_for(tx, templates, packagings).await?;
    for vals in values {
        if let Some(Cell::Str(barcode)) = vals.get("barcode")
            && !barcode.is_empty()
        {
            keys.push(barcode.to_string());
        }
    }
    lock(tx, &mut keys).await?;
    Ok(keys)
}

async fn keys_for(
    tx: &mut Transaction,
    templates: &[i64],
    packagings: &[i64],
) -> Result<Vec<String>> {
    let rows = tx
        .query(
            "SELECT barcode FROM product_template WHERE id = ANY($1)
        UNION SELECT barcode FROM product_product WHERE product_tmpl_id = ANY($1)
        UNION SELECT barcode FROM product_uom WHERE id = ANY($2)
        UNION SELECT c.barcode FROM product_barcode_claim c WHERE c.template_id = ANY($1)
            OR c.packaging_id = ANY($2) OR c.variant_id IN
                (SELECT id FROM product_product WHERE product_tmpl_id = ANY($1))",
            &[&templates, &packagings],
        )
        .await?;
    let mut keys = Vec::new();
    for row in rows {
        let value: Option<String> = row
            .try_get(0)
            .map_err(|e| Error::db("Barcode scope key decode failed.").with_source(e))?;
        if let Some(value) = value.filter(|s| !s.is_empty()) {
            keys.push(value);
        }
    }
    Ok(keys)
}

async fn lock(tx: &mut Transaction, keys: &mut Vec<String>) -> Result<()> {
    keys.sort_unstable();
    keys.dedup();
    if !keys.is_empty() {
        tx.execute(
            "INSERT INTO product_barcode_scope (barcode)
            SELECT key FROM unnest($1::text[]) AS key ORDER BY key COLLATE \"C\"
            ON CONFLICT (barcode) DO UPDATE SET barcode = EXCLUDED.barcode",
            &[&keys.as_slice()],
        )
        .await?;
    }
    Ok(())
}

/// Final stored state, claim state and error all belong to the source mutation's transaction.
pub(crate) async fn reconcile(
    tx: &mut Transaction,
    env: &Env,
    templates: &[i64],
    packagings: &[i64],
    mut keys: Vec<String>,
) -> Result<()> {
    tx.flush().await?;
    keys.extend(keys_for(tx, templates, packagings).await?);
    lock(tx, &mut keys).await?;
    if keys.is_empty() {
        return Ok(());
    }
    validate(tx, env, Keys::Listed(&keys), false).await?;
    tx.execute(
        "DELETE FROM product_barcode_claim WHERE barcode = ANY($1)",
        &[&keys],
    )
    .await?;
    write_claims(tx, Keys::Listed(&keys)).await
}

async fn write_claims(tx: &mut Transaction, keys: Keys<'_>) -> Result<()> {
    let sql = format!(
        "INSERT INTO product_barcode_claim
        (barcode, company_id, variant_id, template_id, packaging_id)
        SELECT barcode, company_id, variant_id, template_id, packaging_id FROM ({}) h",
        holders(keys.sql())
    );
    match keys {
        Keys::Listed(listed) => tx.execute(&sql, &[&listed]).await?,
        Keys::All => tx.execute(&sql, &[]).await?,
    };
    Ok(())
}

/// Stands for every holder of one barcode that the reader cannot read, whatever their number.
pub const HIDDEN_HOLDERS: &str = "one or more records you cannot see";

/// Resolve all actual conflicts before returning any diagnostic, without leaking privileged labels.
async fn validate(tx: &mut Transaction, env: &Env, keys: Keys<'_>, migration: bool) -> Result<()> {
    // Match the supplier migration's diagnostic convention: ten identities and the total.
    // This caps an operator error payload, not a configurable record/admission limit.
    let (count_column, sample_clause) = if migration {
        (", count(*) OVER () AS total", " LIMIT 10")
    } else {
        ("", "")
    };
    let sql = format!(
        "WITH holders AS ({}) SELECT h.model, h.holder, h.barcode, h.company_id{count_column}
        FROM holders h WHERE EXISTS (SELECT 1 FROM holders other
            WHERE other.barcode = h.barcode AND (other.model, other.holder) <> (h.model, h.holder)
            AND (other.company_id IS NULL OR h.company_id IS NULL OR other.company_id = h.company_id
                OR (other.packaging_id IS NOT NULL AND h.packaging_id IS NOT NULL)))
        ORDER BY h.barcode COLLATE \"C\", h.model, h.holder{sample_clause}",
        holders(keys.sql())
    );
    let rows = match keys {
        Keys::Listed(listed) => tx.query(&sql, &[&listed]).await?,
        Keys::All => tx.query(&sql, &[]).await?,
    };
    if rows.is_empty() {
        return Ok(());
    }
    let sampled = rows.len();
    let total = if migration {
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("Barcode migration conflict count is missing."))?;
        Some(row.try_get::<_, i64>(4).map_err(|e| {
            Error::db("Barcode migration conflict count decode failed.").with_source(e)
        })?)
    } else {
        None
    };
    let mut groups: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
    let reader = env
        .label_reader()
        .with_context([("active_test", Value::Bool(false))]);
    for row in rows {
        let model: String = row
            .try_get(0)
            .map_err(|e| Error::db("Barcode holder model decode failed.").with_source(e))?;
        let id: i64 = row
            .try_get(1)
            .map_err(|e| Error::db("Barcode holder identity decode failed.").with_source(e))?;
        let barcode: String = row
            .try_get(2)
            .map_err(|e| Error::db("Barcode holder value decode failed.").with_source(e))?;
        let (visible, hidden) = groups.entry(barcode).or_default();
        if migration {
            let company: Option<i64> = row
                .try_get(3)
                .map_err(|e| Error::db("Barcode migration scope decode failed.").with_source(e))?;
            let scope = match company {
                Some(id) => format!("company {id}"),
                None => "shared".to_owned(),
            };
            visible.push(format!("{model} row {id} ({scope})"));
            continue;
        }
        let record = reader.records(&model)?.browse(vec![id])?;
        match record.cell(tx, "display_name").await {
            Ok(Cell::Str(name)) => visible.push(format!(
                "{}: {name}",
                if model == "product.uom" {
                    "packaging"
                } else {
                    "product"
                }
            )),
            Ok(other) => {
                return Err(Error::programming(format!(
                    "Barcode holder display_name returned {other:?}, expected text."
                )));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    erp_sdk::ErrorKind::Access | erp_sdk::ErrorKind::Missing
                ) =>
            {
                *hidden = true;
            }
            Err(error) => return Err(error),
        }
    }
    let mut lines = Vec::with_capacity(groups.len());
    for (barcode, (visible, hidden)) in groups {
        let mut labels = visible;
        if hidden {
            // One fixed phrase: neither the number nor the kind of unreadable holders is told.
            labels.push(HIDDEN_HOLDERS.to_owned());
        }
        lines.push(format!("- \"{barcode}\" is held by {}", labels.join(", ")));
    }
    let prefix = match total {
        Some(total) => format!(
            "Barcode claim migration refused {total} conflicting rows (first {sampled} identities)"
        ),
        None => "These barcodes are already in use".to_owned(),
    };
    Err(Error::validation(format!(
        "{prefix}:\n\n{}\n\nHolders you have no access to are not named.",
        lines.join("\n")
    )))
}

/// Backfill under lifecycle write exclusion, set-based on the server and validated once.
/// A template barcode that its own variant also holds while the template has several variants
/// (the old mirror counted active variants only) is the variant's: the template copy is cleared
/// first, by the same rule as [`drop_stale_mirrors`]. Any other duplicate refuses the upgrade;
/// no row is deleted and no value is normalized.
pub(crate) async fn backfill(tx: &mut Transaction) -> Result<()> {
    tx.batch(
        "LOCK TABLE product_template, product_product, product_uom IN SHARE ROW EXCLUSIVE MODE",
    )
    .await?;
    tx.flush().await?;
    let uid = tx.env().uid();
    let cleared = tx
        .execute(
            &format!("UPDATE product_template t SET {CLEAR_MIRROR} WHERE {STALE_MIRROR}"),
            &[&uid],
        )
        .await?;
    if cleared > 0 {
        tracing::warn!(
            target: "erp::migration",
            templates = cleared,
            "product 1.5: cleared the barcode of templates whose own variant holds the same barcode; the variant keeps it"
        );
        tx.invalidate_model("product.template", Some(&CLEARED_FIELDS))
            .await?;
    }
    tx.execute(
        "INSERT INTO product_barcode_scope (barcode)
        SELECT key FROM (SELECT barcode FROM product_template WHERE barcode <> ''
            UNION SELECT barcode FROM product_product WHERE barcode <> ''
            UNION SELECT barcode FROM product_uom WHERE barcode <> '') k(key)
        ORDER BY key COLLATE \"C\"
        ON CONFLICT (barcode) DO UPDATE SET barcode = EXCLUDED.barcode",
        &[],
    )
    .await?;
    let env = tx.env().clone();
    validate(tx, &env, Keys::All, true).await?;
    tx.execute("DELETE FROM product_barcode_claim", &[]).await?;
    write_claims(tx, Keys::All).await
}

/// Families affected by variant writes, including the old family when reparenting.
pub(crate) async fn variant_families(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<i64>> {
    let rows = tx
        .query(
            "SELECT DISTINCT product_tmpl_id FROM product_product WHERE id = ANY($1)",
            &[&ids],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            row.try_get(0)
                .map_err(|e| Error::db("Barcode family decode failed.").with_source(e))
        })
        .collect()
}
