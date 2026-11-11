//! Tenant-local durable UI publication. Every mutation uses the caller's install transaction.
//!
//! This private service schema belongs to `web`, not to core DDL or a public CRUD model. It
//! stores raw bytea (not base64 model fields) and exposes no RPC. The host grants runtime SELECT
//! only; installation/upgrade owns schema and publication..
use super::assembly::{AssembledBundle, Blob, MAX_BUNDLE_BYTES};
use erp_sdk::{Error, Result, Transaction};

/// Asset publication lock, database-local. Install callers already hold the module lock; startup
/// takes that lock first too. Keep the lock ordering module → asset in every host adapter.
pub const ASSET_LOCK: (i32, i32) = (0x4552_5046, 5);
/// Retain compatible previous manifests for open tabs (T-AST.2); expired references are collected
/// during the next lifecycle publication. Other-runtime bytes are never admitted for serving.
pub const GRACE_SECONDS: i64 = 86_400;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> Error {
    Error::validation(message)
}
fn stored(error: impl std::fmt::Display) -> Error {
    invalid(format!("invalid asset blob or manifest: {error}"))
}

/// Create the web addon's service tables in its initial install/upgrade transaction.
/// Startup and request readers never call this function.
/// # Errors
/// Propagates all database/schema errors; no partial publication is committed here.
pub async fn initialize(tx: &mut Transaction) -> Result<()> {
    tx.batch("CREATE TABLE IF NOT EXISTS ir_ui_asset_blob (
        sha256 text PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
        bytes bytea NOT NULL CHECK (octet_length(bytes) <= 16777216),
        gzip bytea NOT NULL CHECK (octet_length(gzip) <= 17825792),
        brotli bytea NOT NULL CHECK (octet_length(brotli) <= 17825792),
        media_type text NOT NULL CHECK (octet_length(media_type) BETWEEN 1 AND 128),
        extension text NOT NULL CHECK (octet_length(extension) BETWEEN 1 AND 8));
        CREATE TABLE IF NOT EXISTS ir_ui_asset_bundle (
        bundle_key text PRIMARY KEY CHECK (bundle_key ~ '^[0-9a-f]{64}$'),
        language text NOT NULL CHECK (octet_length(language) BETWEEN 1 AND 64),
        lazy_group text NOT NULL CHECK (octet_length(lazy_group) BETWEEN 1 AND 64),
        compiler_version text NOT NULL CHECK (octet_length(compiler_version) BETWEEN 1 AND 128),
        runtime_version text NOT NULL CHECK (octet_length(runtime_version) BETWEEN 1 AND 128),
        manifest bytea NOT NULL CHECK (octet_length(manifest) <= 16777216),
        blob_hashes text[] NOT NULL CHECK (cardinality(blob_hashes) BETWEEN 1 AND 40001 AND array_ndims(blob_hashes)=1 AND array_lower(blob_hashes,1)=1),
        expires_at timestamptz);
        CREATE UNIQUE INDEX IF NOT EXISTS ir_ui_asset_bundle_current
        ON ir_ui_asset_bundle (language,lazy_group) WHERE expires_at IS NULL;")
        .await
}
async fn lock(tx: &mut Transaction) -> Result<()> {
    tx.query(
        "SELECT pg_advisory_xact_lock($1,$2)",
        &[&ASSET_LOCK.0, &ASSET_LOCK.1],
    )
    .await?;
    Ok(())
}

/// Atomically replace a current manifest and collect unreferenced tenant-local blobs. All
/// assembly has completed before entry. The supplied transaction owns commit/rollback.
/// # Errors
/// Database, conflicting hash metadata, oversized manifest or invalid assembled content.
pub async fn publish(tx: &mut Transaction, bundle: &AssembledBundle) -> Result<()> {
    publish_with_limits(tx, bundle, RetainedLimits::default()).await
}
async fn publish_with_limits(
    tx: &mut Transaction,
    bundle: &AssembledBundle,
    limits: RetainedLimits,
) -> Result<()> {
    let manifest = bundle.encode_manifest().map_err(stored)?;
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("asset manifest exceeds 16777216 bytes"));
    }
    lock(tx).await?;
    let raw: std::collections::BTreeMap<&str, &Blob> = bundle
        .blobs()
        .iter()
        .map(|blob| (blob.sha256(), blob))
        .collect();
    for blob in raw.values() {
        tx.execute(
            "INSERT INTO ir_ui_asset_blob (sha256,bytes,gzip,brotli,media_type,extension)
            VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (sha256) DO NOTHING",
            &[
                &blob.sha256(),
                &blob.bytes(),
                &blob.gzip(),
                &blob.brotli(),
                &blob.media_type(),
                &blob.extension(),
            ],
        )
        .await?;
        let rows = tx.query("SELECT CASE WHEN octet_length(bytes)<=16777216 THEN bytes END,CASE WHEN octet_length(gzip)<=17825792 THEN gzip END,CASE WHEN octet_length(brotli)<=17825792 THEN brotli END,CASE WHEN octet_length(media_type) BETWEEN 1 AND 128 THEN media_type END,CASE WHEN octet_length(extension) BETWEEN 1 AND 8 THEN extension END FROM ir_ui_asset_blob WHERE sha256=$1", &[&blob.sha256()]).await?;
        let [row] = rows.as_slice() else {
            return Err(invalid("asset blob disappeared during publication"));
        };
        let bytes = row
            .try_get::<_, Option<Vec<u8>>>(0)
            .map_err(stored)?
            .ok_or_else(|| invalid("asset blob representation exceeds bounded admission"))?;
        let gzip = row
            .try_get::<_, Option<Vec<u8>>>(1)
            .map_err(stored)?
            .ok_or_else(|| invalid("asset blob representation exceeds bounded admission"))?;
        let brotli = row
            .try_get::<_, Option<Vec<u8>>>(2)
            .map_err(stored)?
            .ok_or_else(|| invalid("asset blob representation exceeds bounded admission"))?;
        let media = row
            .try_get::<_, Option<String>>(3)
            .map_err(stored)?
            .ok_or_else(|| invalid("asset blob metadata exceeds bounded admission"))?;
        let extension = row
            .try_get::<_, Option<String>>(4)
            .map_err(stored)?
            .ok_or_else(|| invalid("asset blob metadata exceeds bounded admission"))?;
        let admitted = Blob::from_stored(
            blob.sha256().to_owned(),
            bytes,
            gzip,
            brotli,
            media,
            extension,
        )
        .map_err(stored)?;
        // The row owns bytes; each manifest owns its artifact MIME/extension declarations.
        if admitted.bytes() != blob.bytes() {
            return Err(invalid(format!(
                "asset blob {} conflicts with stored content or media type",
                blob.sha256()
            )));
        }
    }
    // Re-admitting exactly the same key retains the generation and still verifies its content.
    let hashes: Vec<&str> = raw.keys().copied().collect();
    tx.execute("UPDATE ir_ui_asset_bundle SET expires_at=CURRENT_TIMESTAMP + $3::bigint * INTERVAL '1 second'
        WHERE language=$1 AND lazy_group=$2 AND expires_at IS NULL AND bundle_key<>$4",
        &[&bundle.language(), &bundle.group(), &GRACE_SECONDS, &bundle.key()]).await?;
    tx.execute("INSERT INTO ir_ui_asset_bundle (bundle_key,language,lazy_group,compiler_version,runtime_version,manifest,blob_hashes)
        VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (bundle_key) DO UPDATE SET expires_at=NULL
        WHERE ir_ui_asset_bundle.manifest=EXCLUDED.manifest AND ir_ui_asset_bundle.blob_hashes=EXCLUDED.blob_hashes
        AND ir_ui_asset_bundle.compiler_version=EXCLUDED.compiler_version AND ir_ui_asset_bundle.runtime_version=EXCLUDED.runtime_version
        AND ir_ui_asset_bundle.language=EXCLUDED.language AND ir_ui_asset_bundle.lazy_group=EXCLUDED.lazy_group",
        &[&bundle.key(), &bundle.language(), &bundle.group(), &bundle.compiler(), &bundle.runtime(), &manifest, &hashes]).await?;
    let verify = format!(
        "SELECT CASE WHEN octet_length(manifest)<=16777216 THEN manifest END,CASE WHEN ({REFERENCES_BOUND_SQL}) THEN blob_hashes END,CASE WHEN octet_length(compiler_version) BETWEEN 1 AND 128 THEN compiler_version END,CASE WHEN octet_length(runtime_version) BETWEEN 1 AND 128 THEN runtime_version END,CASE WHEN octet_length(language) BETWEEN 1 AND 64 THEN language END,CASE WHEN octet_length(lazy_group) BETWEEN 1 AND 64 THEN lazy_group END FROM ir_ui_asset_bundle WHERE bundle_key=$1 AND expires_at IS NULL"
    );
    let rows = tx.query(&verify, &[&bundle.key()]).await?;
    let conflict =
        || invalid("asset bundle key conflicts with stored manifest identity or references");
    let [row] = rows.as_slice() else {
        return Err(conflict());
    };
    let actual = row
        .try_get::<_, Option<Vec<u8>>>(0)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    let actual_hashes = row
        .try_get::<_, Option<Vec<String>>>(1)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    let compiler = row
        .try_get::<_, Option<String>>(2)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    let runtime = row
        .try_get::<_, Option<String>>(3)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    let language = row
        .try_get::<_, Option<String>>(4)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    let group = row
        .try_get::<_, Option<String>>(5)
        .map_err(stored)?
        .ok_or_else(conflict)?;
    if actual != manifest
        || !actual_hashes
            .iter()
            .map(String::as_str)
            .eq(hashes.iter().copied())
        || compiler != bundle.compiler()
        || runtime != bundle.runtime()
        || language != bundle.language()
        || group != bundle.group()
    {
        return Err(conflict());
    }
    collect_locked(tx).await?;
    // Admission is part of publication, before the caller may commit. A bundle that would make
    // its compatible grace history unservable refuses the complete install/upgrade transaction.
    retained_with_limits(
        tx,
        bundle.language(),
        bundle.group(),
        bundle.runtime(),
        limits,
    )
    .await?;
    Ok(())
}

/// Collect expired manifests and blobs referenced by no remaining manifest, in this database.
/// # Errors
/// Database failures are returned and leave rollback to the caller.
pub async fn collect(tx: &mut Transaction) -> Result<()> {
    lock(tx).await?;
    collect_locked(tx).await
}
async fn collect_locked(tx: &mut Transaction) -> Result<()> {
    tx.execute(
        "DELETE FROM ir_ui_asset_bundle WHERE expires_at <= CURRENT_TIMESTAMP",
        &[],
    )
    .await?;
    tx.execute("DELETE FROM ir_ui_asset_blob b WHERE NOT EXISTS (SELECT 1 FROM ir_ui_asset_bundle m WHERE b.sha256=ANY(m.blob_hashes))", &[]).await?;
    Ok(())
}

/// Read and verify the current tenant-local generation for startup or registry publication.
/// A request never invokes this service to build or repair a bundle.
/// # Errors
/// Incompatible runtime, malformed storage, corrupt bytes or database failure.
pub async fn current(
    tx: &mut Transaction,
    language: &str,
    group: &str,
    runtime: &str,
) -> Result<Option<AssembledBundle>> {
    let sql = format!(
        "SELECT {} FROM ir_ui_asset_bundle WHERE language=$1 AND lazy_group=$2 AND expires_at IS NULL",
        metadata_projection("TRUE")
    );
    let rows = tx.query(&sql, &[&language, &group]).await?;
    let row = match rows.as_slice() {
        [] => return Ok(None),
        [row] => row,
        _ => return Err(invalid("multiple current asset manifests")),
    };
    let metadata = metadata_row(row)?;
    decode_row(tx, metadata, language, group, runtime, MAX_BUNDLE_BYTES)
        .await
        .map(|(bundle, _)| Some(bundle))
}
// A CASE gate prevents oversized/corrupt varlena payloads from reaching the client decoder.
// The nested reference check runs only after cardinality/dimensions pass; each hash is bounded
// before the regular expression is evaluated. Existing/restored tables need not have new CHECKs.
const REFERENCES_BOUND_SQL: &str = "CASE WHEN cardinality(blob_hashes) BETWEEN 1 AND 40001 AND array_ndims(blob_hashes)=1 AND array_lower(blob_hashes,1)=1 THEN NOT EXISTS (SELECT 1 FROM unnest(blob_hashes) AS r(hash) WHERE CASE WHEN octet_length(hash)=64 THEN hash ~ '^[0-9a-f]{64}$' ELSE FALSE END IS NOT TRUE) ELSE FALSE END";
fn metadata_projection(guard: &str) -> String {
    let refs = REFERENCES_BOUND_SQL;
    format!("CASE WHEN ({guard}) THEN CASE WHEN octet_length(bundle_key)=64 THEN bundle_key END END,
      CASE WHEN ({guard}) THEN CASE WHEN octet_length(runtime_version) BETWEEN 1 AND 128 THEN runtime_version END END,
      CASE WHEN ({guard}) THEN CASE WHEN octet_length(compiler_version) BETWEEN 1 AND 128 THEN compiler_version END END,
      CASE WHEN ({guard}) THEN CASE WHEN octet_length(manifest)<=16777216 THEN manifest END END,
      CASE WHEN ({guard}) THEN CASE WHEN ({refs}) THEN blob_hashes END END")
}
fn metadata_row(row: &erp_sdk::Row) -> Result<StoredManifest> {
    let missing = || invalid("asset manifest metadata exceeds bounded admission");
    Ok(StoredManifest {
        key: row
            .try_get::<_, Option<String>>(0)
            .map_err(stored)?
            .ok_or_else(missing)?,
        runtime: row
            .try_get::<_, Option<String>>(1)
            .map_err(stored)?
            .ok_or_else(missing)?,
        compiler: row
            .try_get::<_, Option<String>>(2)
            .map_err(stored)?
            .ok_or_else(missing)?,
        manifest: row
            .try_get::<_, Option<Vec<u8>>>(3)
            .map_err(stored)?
            .ok_or_else(missing)?,
        hashes: row
            .try_get::<_, Option<Vec<String>>>(4)
            .map_err(stored)?
            .ok_or_else(missing)?,
    })
}
struct StoredManifest {
    key: String,
    runtime: String,
    compiler: String,
    manifest: Vec<u8>,
    hashes: Vec<String>,
}
async fn decode_row(
    tx: &mut Transaction,
    metadata: StoredManifest,
    language: &str,
    group: &str,
    runtime: &str,
    remaining_bytes: usize,
) -> Result<(AssembledBundle, usize)> {
    let actual_runtime = metadata.runtime;
    if actual_runtime != runtime {
        return Err(invalid(format!(
            "asset runtime '{actual_runtime}' does not match '{runtime}'"
        )));
    }
    let manifest = metadata.manifest;
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("asset manifest exceeds 16777216 bytes"));
    }
    let hashes = metadata.hashes;
    if hashes.len() > 40_001 {
        return Err(invalid("asset manifest has too many blob references"));
    }
    let references = AssembledBundle::manifest_artifact_hashes(&manifest).map_err(stored)?;
    let expected: std::collections::BTreeSet<_> = references.iter().map(String::as_str).collect();
    if !hashes
        .iter()
        .map(String::as_str)
        .eq(expected.iter().copied())
    {
        return Err(invalid(
            "asset manifest reference set differs from stored row",
        ));
    }
    let sizes = tx.query("SELECT count(*),COALESCE(sum(octet_length(b.bytes)::bigint+octet_length(b.gzip)::bigint+octet_length(b.brotli)::bigint),0)::bigint FROM unnest($1::text[]) AS r(hash) JOIN ir_ui_asset_blob b ON b.sha256=r.hash", &[&references]).await?;
    let [sizes] = sizes.as_slice() else {
        return Err(invalid("asset blob size admission returned no row"));
    };
    let count: i64 = sizes.try_get(0).map_err(stored)?;
    let total: i64 = sizes.try_get(1).map_err(stored)?;
    if count != references.len() as i64 || total < 0 {
        return Err(invalid(
            "asset blob references missing or exceed bounded storage admission",
        ));
    }
    let total = usize::try_from(total).map_err(stored)?;
    if total > remaining_bytes {
        return Err(invalid(
            "retained asset representation bytes exceed bounded admission",
        ));
    }
    let rows = tx.query("SELECT sha256,CASE WHEN octet_length(bytes)<=16777216 THEN bytes END,CASE WHEN octet_length(gzip)<=17825792 THEN gzip END,CASE WHEN octet_length(brotli)<=17825792 THEN brotli END,CASE WHEN octet_length(media_type) BETWEEN 1 AND 128 THEN media_type END,CASE WHEN octet_length(extension) BETWEEN 1 AND 8 THEN extension END FROM ir_ui_asset_blob WHERE sha256=ANY($1) ORDER BY sha256", &[&hashes]).await?;
    let mut blobs = Vec::with_capacity(rows.len());
    for row in rows {
        blobs.push(
            Blob::from_stored(
                row.try_get(0).map_err(stored)?,
                row.try_get::<_, Option<Vec<u8>>>(1)
                    .map_err(stored)?
                    .ok_or_else(|| {
                        invalid("asset blob representation exceeds bounded admission")
                    })?,
                row.try_get::<_, Option<Vec<u8>>>(2)
                    .map_err(stored)?
                    .ok_or_else(|| {
                        invalid("asset blob representation exceeds bounded admission")
                    })?,
                row.try_get::<_, Option<Vec<u8>>>(3)
                    .map_err(stored)?
                    .ok_or_else(|| {
                        invalid("asset blob representation exceeds bounded admission")
                    })?,
                row.try_get::<_, Option<String>>(4)
                    .map_err(stored)?
                    .ok_or_else(|| invalid("asset blob metadata exceeds bounded admission"))?,
                row.try_get::<_, Option<String>>(5)
                    .map_err(stored)?
                    .ok_or_else(|| invalid("asset blob metadata exceeds bounded admission"))?,
            )
            .map_err(stored)?,
        );
    }
    let bundle = AssembledBundle::decode_manifest(&manifest, blobs).map_err(stored)?;
    let stored_key = metadata.key;
    let stored_compiler = metadata.compiler;
    if bundle.key() != stored_key
        || bundle.compiler() != stored_compiler
        || bundle.runtime() != runtime
        || bundle.language() != language
        || bundle.group() != group
    {
        return Err(invalid(
            "asset manifest identity differs from selected database slot",
        ));
    }
    Ok((bundle, total))
}

/// Read current `(compiler, runtime, bundle key)` metadata without admitting its bytes. Startup
/// uses this only under lifecycle admission to choose upgrade versus incompatibility refusal.
/// # Errors
/// Malformed state or database failure.
pub async fn current_identity(
    tx: &mut Transaction,
    language: &str,
    group: &str,
) -> Result<Option<(String, String, String)>> {
    let rows = tx.query("SELECT CASE WHEN octet_length(compiler_version) BETWEEN 1 AND 128 THEN compiler_version END,CASE WHEN octet_length(runtime_version) BETWEEN 1 AND 128 THEN runtime_version END,CASE WHEN octet_length(bundle_key)=64 THEN bundle_key END FROM ir_ui_asset_bundle WHERE language=$1 AND lazy_group=$2 AND expires_at IS NULL", &[&language, &group]).await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some((
            row.try_get::<_, Option<String>>(0)
                .map_err(stored)?
                .ok_or_else(|| invalid("asset manifest identity exceeds bounded admission"))?,
            row.try_get::<_, Option<String>>(1)
                .map_err(stored)?
                .ok_or_else(|| invalid("asset manifest identity exceeds bounded admission"))?,
            row.try_get::<_, Option<String>>(2)
                .map_err(stored)?
                .ok_or_else(|| invalid("asset manifest identity exceeds bounded admission"))?,
        ))),
        _ => Err(invalid("multiple current asset manifests")),
    }
}

/// Publish runtime read grants while retaining lifecycle-only writes to web service storage.
/// # Errors
/// Invalid role name or failed grant/revoke; inherited effective permissions remain the host's
/// responsibility and are verified by its runtime-role admission query.
pub async fn grant_runtime_read(tx: &mut Transaction, role: &str) -> Result<()> {
    if role.is_empty() || role.len() > 63 || role.chars().any(char::is_control) {
        return Err(invalid("invalid UI asset runtime role"));
    }
    let role = format!("\"{}\"", role.replace('"', "\"\""));
    tx.batch(&format!("REVOKE ALL ON ir_ui_asset_blob,ir_ui_asset_bundle FROM {role}; GRANT SELECT ON ir_ui_asset_blob,ir_ui_asset_bundle TO {role}")).await
}

/// Read current and unexpired previous manifests for one compatible runtime. Their retained
/// references permit open tabs to fetch old content hashes during the fixed grace interval.
/// # Errors
/// Missing/incompatible current generation, excessive history, corruption or database failure.
pub async fn retained(
    tx: &mut Transaction,
    language: &str,
    group: &str,
    runtime: &str,
) -> Result<Vec<AssembledBundle>> {
    retained_with_limits(tx, language, group, runtime, RetainedLimits::default()).await
}
/// Read current and retained manifests within the host's aggregate representation byte cap.
///
/// The cap includes identity, gzip and Brotli bytes and charges a shared blob again when a
/// second retained bundle materializes it. SQL size preflight refuses excess before fetching
/// that bundle's representation payloads. A host cap above [`MAX_BUNDLE_BYTES`] cannot raise
/// the durable service's hard admission ceiling. This bounds load buffers; the server separately
/// accounts for live resident allocations and response ownership.
/// # Errors
/// Zero cap, missing/incompatible current generation, excessive retained bytes/history, corrupt
/// stored content or a database failure. A zero cap never means unlimited.
pub async fn retained_bounded(
    tx: &mut Transaction,
    language: &str,
    group: &str,
    runtime: &str,
    representation_bytes: usize,
) -> Result<Vec<AssembledBundle>> {
    let limits = RetainedLimits {
        artifact_bytes: representation_bytes.min(MAX_BUNDLE_BYTES),
        ..RetainedLimits::default()
    };
    retained_with_limits(tx, language, group, runtime, limits).await
}

#[derive(Clone, Copy)]
struct RetainedLimits {
    manifest_bytes: usize,
    references: usize,
    artifact_bytes: usize,
    serving_raw_bytes: usize,
    serving_urls: usize,
}
impl Default for RetainedLimits {
    fn default() -> Self {
        Self {
            manifest_bytes: MAX_MANIFEST_BYTES,
            references: 40_001,
            artifact_bytes: MAX_BUNDLE_BYTES,
            // Functional protocol bounds mirrored from erp-server AssetGenerationBuilder::asset.
            serving_raw_bytes: 128 * 1024 * 1024,
            serving_urls: 40_001,
        }
    }
}
async fn retained_with_limits(
    tx: &mut Transaction,
    language: &str,
    group: &str,
    runtime: &str,
    limits: RetainedLimits,
) -> Result<Vec<AssembledBundle>> {
    if limits.manifest_bytes == 0
        || limits.manifest_bytes > MAX_MANIFEST_BYTES
        || limits.references == 0
        || limits.references > 40_001
        || limits.artifact_bytes == 0
        || limits.artifact_bytes > MAX_BUNDLE_BYTES
        || limits.serving_raw_bytes == 0
        || limits.serving_raw_bytes > 128 * 1024 * 1024
        || limits.serving_urls == 0
        || limits.serving_urls > 40_001
    {
        return Err(invalid("invalid retained asset admission limits"));
    }
    let current = current_identity(tx, language, group)
        .await?
        .ok_or_else(|| invalid("current asset manifest is missing"))?;
    if current.1 != runtime {
        return Err(invalid(format!(
            "asset runtime '{}' does not match '{runtime}'",
            current.1
        )));
    }
    let guard = format!(
        "totals.generation_count<=256 AND totals.manifest_bytes<={} AND totals.reference_count<={}",
        limits.manifest_bytes, limits.references
    );
    let sql = format!("WITH candidates AS MATERIALIZED (
       SELECT bundle_key,runtime_version,compiler_version,manifest,blob_hashes FROM ir_ui_asset_bundle
       WHERE language=$1 AND lazy_group=$2 AND runtime_version=$3 AND (expires_at IS NULL OR expires_at>CURRENT_TIMESTAMP)
       ORDER BY bundle_key LIMIT 257), totals AS (
       SELECT count(*) AS generation_count,COALESCE(sum(octet_length(manifest)),0)::bigint AS manifest_bytes,
       COALESCE(sum(cardinality(blob_hashes)),0)::bigint AS reference_count FROM candidates)
       SELECT {},totals.generation_count,totals.manifest_bytes,totals.reference_count
       FROM totals LEFT JOIN candidates ON TRUE ORDER BY bundle_key", metadata_projection(&guard));
    let rows = tx.query(&sql, &[&language, &group, &runtime]).await?;
    let Some(first) = rows.first() else {
        return Err(invalid("retained asset admission returned no state"));
    };
    let count: i64 = first.try_get(5).map_err(stored)?;
    let manifest_bytes: i64 = first.try_get(6).map_err(stored)?;
    let references: i64 = first.try_get(7).map_err(stored)?;
    if count > 256 {
        return Err(invalid(
            "retained asset generations exceed bounded admission",
        ));
    }
    if count == 0 {
        return Err(invalid("current asset manifest is missing"));
    }
    if manifest_bytes < 0 || manifest_bytes as u64 > limits.manifest_bytes as u64 {
        return Err(invalid(
            "retained asset manifest bytes exceed bounded admission",
        ));
    }
    if references < 0 || references as u64 > limits.references as u64 {
        return Err(invalid(
            "retained asset references exceed bounded admission",
        ));
    }
    let mut bundles = Vec::with_capacity(rows.len());
    let mut remaining = limits.artifact_bytes;
    let mut serving = std::collections::BTreeSet::new();
    let mut serving_bytes = 0usize;
    for row in rows {
        let metadata = metadata_row(&row)?;
        let (bundle, cost) = decode_row(tx, metadata, language, group, runtime, remaining).await?;
        remaining = remaining.checked_sub(cost).ok_or_else(|| {
            invalid("retained asset representation bytes exceed bounded admission")
        })?;
        // Count full hash+extension URLs exactly as the server generation does. Shared raw
        // rows with different declared media still occupy distinct serving paths.
        for artifact in bundle.blobs() {
            if serving.insert(artifact.url()) {
                serving_bytes = serving_bytes
                    .checked_add(artifact.bytes().len())
                    .ok_or_else(|| invalid("retained asset union exceeds serving admission"))?;
                if serving.len() > limits.serving_urls || serving_bytes > limits.serving_raw_bytes {
                    return Err(invalid("retained asset union exceeds serving admission"));
                }
            }
        }
        bundles.push(bundle);
    }
    Ok(bundles)
}

#[cfg(test)]
mod retained_admission_tests {
    use super::*;
    use crate::assets::assembly::{
        AddonAssets, AssemblyOptions, AssetInput, MAX_BUNDLE_BYTES, assemble,
    };
    use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
    use erp_testkit::Harness;

    fn assembled(compiler: &str) -> AssembledBundle {
        let mut addon = AddonAssets::new("web", "1", "runtime-1").unwrap();
        addon
            .push(
                AssetInput::module(
                    "static/dist/main.js",
                    "web.fixture",
                    b"export function register() {}".to_vec(),
                )
                .unwrap(),
            )
            .unwrap();
        addon.set_entry("web.fixture").unwrap();
        assemble(
            &[addon],
            &AssemblyOptions::new(
                compiler,
                "runtime-1",
                "en_US",
                "main",
                CompileOptions::new(BuildMode::Prod),
            )
            .unwrap(),
        )
        .unwrap()
    }
    async fn history() -> (Harness, AssembledBundle, AssembledBundle) {
        let h = Harness::builder([&erp_addon_base::ADDON, &crate::ADDON])
            .without_schema()
            .build()
            .await
            .unwrap();
        let first = assembled("compiler-a");
        let second = assembled("compiler-b");
        let mut tx = h.begin().await.unwrap();
        initialize(&mut tx).await.unwrap();
        publish(&mut tx, &first).await.unwrap();
        publish(&mut tx, &second).await.unwrap();
        tx.commit().await.unwrap();
        (h, first, second)
    }
    #[tokio::test]
    async fn combined_retained_manifest_budget_refuses_before_blob_decode() {
        let (h, first, second) = history().await;
        let cap = first
            .encode_manifest()
            .unwrap()
            .len()
            .max(second.encode_manifest().unwrap().len());
        let limits = RetainedLimits {
            manifest_bytes: cap,
            references: 40_001,
            artifact_bytes: MAX_BUNDLE_BYTES,
            ..RetainedLimits::default()
        };
        let mut tx = h.begin().await.unwrap();
        // This would cause a different error if the service reached raw representation decoding.
        tx.execute(
            "UPDATE ir_ui_asset_blob SET bytes=$1",
            &[&b"corrupt".to_vec()],
        )
        .await
        .unwrap();
        let error = retained_with_limits(&mut tx, "en_US", "main", "runtime-1", limits)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "retained asset manifest bytes exceed bounded admission"
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn retained_budget_charges_repeated_shared_content_across_generations() {
        let (h, first, second) = history().await;
        assert_eq!(first.blobs(), second.blobs());
        let one: usize = first
            .blobs()
            .iter()
            .map(|b| b.bytes().len() + b.gzip().len() + b.brotli().len())
            .sum();
        let limits = RetainedLimits {
            manifest_bytes: MAX_MANIFEST_BYTES,
            references: 40_001,
            artifact_bytes: one + 1,
            ..RetainedLimits::default()
        };
        let mut tx = h.begin().await.unwrap();
        let error = retained_with_limits(&mut tx, "en_US", "main", "runtime-1", limits)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "retained asset representation bytes exceed bounded admission"
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn publication_refuses_unservable_history_before_commit() {
        let h = Harness::builder([&erp_addon_base::ADDON, &crate::ADDON])
            .without_schema()
            .build()
            .await
            .unwrap();
        let first = assembled("compiler-a");
        let next = assembled("compiler-b");
        let one: usize = first
            .blobs()
            .iter()
            .map(|b| b.bytes().len() + b.gzip().len() + b.brotli().len())
            .sum();
        let mut tx = h.begin().await.unwrap();
        initialize(&mut tx).await.unwrap();
        publish(&mut tx, &first).await.unwrap();
        tx.commit().await.unwrap();
        let limits = RetainedLimits {
            artifact_bytes: one + 1,
            ..RetainedLimits::default()
        };
        let mut tx = h.begin().await.unwrap();
        let error = publish_with_limits(&mut tx, &next, limits)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "retained asset representation bytes exceed bounded admission"
        );
        tx.rollback().await.unwrap();
        let mut tx = h.begin().await.unwrap();
        assert_eq!(
            current(&mut tx, "en_US", "main", "runtime-1")
                .await
                .unwrap()
                .unwrap(),
            first
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn host_representation_budget_refuses_before_corrupt_blob_decode() {
        let (h, _, _) = history().await;
        let mut tx = h.begin().await.unwrap();
        // A decoder would reject this identity with a different error. A low host cap must
        // refuse using the SQL size preflight before any corrupted representation is decoded.
        tx.execute(
            "UPDATE ir_ui_asset_blob SET bytes=$1",
            &[&b"corrupt".to_vec()],
        )
        .await
        .unwrap();
        let error = retained_bounded(&mut tx, "en_US", "main", "runtime-1", 1)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "retained asset representation bytes exceed bounded admission"
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn host_representation_budget_exact_fit_zero_and_hard_ceiling() {
        let (h, first, second) = history().await;
        let bytes: usize = [&first, &second]
            .iter()
            .flat_map(|bundle| bundle.blobs())
            .map(|blob| blob.bytes().len() + blob.gzip().len() + blob.brotli().len())
            .sum();
        let mut tx = h.begin().await.unwrap();
        let exact = retained_bounded(&mut tx, "en_US", "main", "runtime-1", bytes)
            .await
            .unwrap();
        assert_eq!(exact.len(), 2);
        assert!(exact.contains(&first));
        assert!(exact.contains(&second));
        let error = retained_bounded(&mut tx, "en_US", "main", "runtime-1", bytes - 1)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(
            error.to_string(),
            "retained asset representation bytes exceed bounded admission"
        );
        let error = retained_bounded(&mut tx, "en_US", "main", "runtime-1", 0)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(error.to_string(), "invalid retained asset admission limits");
        // A host with a larger resident budget still uses the durable service hard ceiling;
        // it neither overflows a conversion nor raises an untrusted-data admission bound.
        assert_eq!(
            retained_bounded(&mut tx, "en_US", "main", "runtime-1", usize::MAX)
                .await
                .unwrap(),
            exact
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}
