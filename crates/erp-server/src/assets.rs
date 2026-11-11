//! Immutable, digest-verified client compiler output. No directory is exposed as a static mount.
mod negotiation;
mod store;
mod verify;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
    sync::Arc,
};
pub use store::AssetStore;
use thiserror::Error;
/// Invalid compiled client input or resident admission failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AssetError {
    /// Invalid, incomplete, oversized or digest-mismatched compiler output.
    #[error("compiled client asset manifest or content is invalid")]
    Invalid,
    /// The shared resident allowance refused this release.
    #[error(transparent)]
    Admission(#[from] PublicationError),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u8,
    assets: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    path: String,
    sha256: String,
    content_type: String,
}
/// Verified operator release bundle, charged to the shared asset store. The manifest carries
/// identity files only; gzip and Brotli forms are built and verified at load (
/// amendment).
pub struct AssetBundle {
    entries: BTreeMap<String, Asset>,
    version: String,
    store: Arc<AssetStore>,
    _metadata: store::Reservation,
}
struct Asset {
    path: String,
    resident: Arc<store::ResidentAsset>,
}
impl AssetBundle {
    /// Verify both release outputs and atomically admit them to the configured shared store.
    /// # Errors
    /// Any shape, file, digest or resident admission refusal; never returns a partial bundle.
    pub fn load(path: &Path, store: &Arc<AssetStore>) -> Result<Self, AssetError> {
        let manifest: Manifest =
            serde_json::from_slice(&read(path, 8192)?).map_err(|_| AssetError::Invalid)?;
        if manifest.schema != 1 || manifest.assets.len() != 2 {
            return Err(AssetError::Invalid);
        }
        let base = path.parent().ok_or(AssetError::Invalid)?;
        let mut sources = BTreeMap::new();
        let mut paths = BTreeMap::new();
        for entry in manifest.assets {
            let (extension, media) = match entry.name.as_str() {
                "client.js" => ("js", "text/javascript; charset=utf-8"),
                "client.css" => ("css", "text/css; charset=utf-8"),
                _ => return Err(AssetError::Invalid),
            };
            if !valid_hash(&entry.sha256)
                || entry.path != format!("client.{}.{extension}", entry.sha256)
                || entry.content_type != media
            {
                return Err(AssetError::Invalid);
            }
            let bytes = read(&base.join(&entry.path), 16 * 1024 * 1024)?;
            // forms are built here with the tenant-assembly parameters and admitted through the
            // same exact-decode verification as stored tenant forms.
            let (gzip, brotli) = encode_release(&bytes)?;
            let source = AssetSource::with_encodings(
                &format!("{}.{extension}", entry.sha256),
                media,
                bytes,
                gzip,
                brotli,
            )?;
            if sources.insert(entry.name.clone(), source).is_some() {
                return Err(AssetError::Invalid);
            }
            paths.insert(entry.name, entry.path);
        }
        if !sources.contains_key("client.js") || !sources.contains_key("client.css") {
            return Err(AssetError::Invalid);
        }
        let metadata = metadata_cost(
            paths
                .iter()
                .flat_map(|(name, path)| [name.as_str(), path.as_str()]),
        )?
        .checked_add(512)
        .ok_or_else(|| {
            refused(
                PublicationErrorKind::Limit,
                "assets.memory_bytes admission limit exceeded",
            )
        })?;
        let (residents, reservation) = store.admit(sources, metadata)?;
        let mut entries = BTreeMap::new();
        let mut digest = Sha256::new();
        for (name, resident) in residents {
            let path = paths.remove(&name).ok_or(AssetError::Invalid)?;
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(path.as_bytes());
            digest.update([0]);
            entries.insert(name, Asset { path, resident });
        }
        Ok(Self {
            entries,
            version: hex(&digest.finalize()),
            store: Arc::clone(store),
            _metadata: reservation,
        })
    }
    /// Content identity of this admitted release.
    pub fn version(&self) -> &str {
        &self.version
    }
    /// Immutable path of an admitted logical asset.
    pub fn path(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(|asset| asset.path.as_str())
    }
    pub(crate) fn response(
        &self,
        path: &str,
        validator: Option<&str>,
        encoding: Option<&str>,
    ) -> Result<Option<axum::response::Response>, PublicationError> {
        let Some(asset) = self.entries.values().find(|asset| asset.path == path) else {
            return Ok(None);
        };
        self.store.touch(&asset.resident.key)?;
        Ok(Some(asset.resident.response(validator, encoding)?))
    }
}
fn read(path: &Path, limit: usize) -> Result<Vec<u8>, AssetError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| AssetError::Invalid)?;
    let meta = file.metadata().map_err(|_| AssetError::Invalid)?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err(AssetError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AssetError::Invalid)?;
    if bytes.len() > limit {
        return Err(AssetError::Invalid);
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Gzip level, mtime and Brotli quality/window of tenant assembly
/// (`addons/web/src/assets/assembly.rs`), so both paths produce the same encoded bytes.
const RELEASE_GZIP_LEVEL: u32 = 9;
const RELEASE_BROTLI_QUALITY: i32 = 11;
const RELEASE_BROTLI_WINDOW: i32 = 22;

/// Deterministic gzip and Brotli forms of one verified release file.
///
/// CPU-bound and synchronous. `AssetBundle::load` runs once while the server is composed,
/// before it accepts requests, so no request ever waits on this; the cost is not measured
/// .
fn encode_release(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), AssetError> {
    use std::io::Write;
    let failed = |stage: &'static str| {
        move |error: std::io::Error| {
            AssetError::Admission(refused(
                PublicationErrorKind::Representation,
                format!("release asset {stage} compression failed: {error}"),
            ))
        }
    };
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::new(RELEASE_GZIP_LEVEL));
    encoder.write_all(bytes).map_err(failed("gzip"))?;
    let gzip = encoder.finish().map_err(failed("gzip"))?;
    let mut brotli = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: RELEASE_BROTLI_QUALITY,
        lgwin: RELEASE_BROTLI_WINDOW,
        ..Default::default()
    };
    brotli::BrotliCompress(&mut std::io::Cursor::new(bytes), &mut brotli, &params)
        .map_err(failed("Brotli"))?;
    Ok((gzip, brotli))
}

/// A rejected publication or request identity. The detail names the failed admission boundary.
#[derive(Debug, Error)]
#[error("asset publication refused: {detail}")]
#[non_exhaustive]
pub struct PublicationError {
    kind: PublicationErrorKind,
    detail: String,
}
/// Stable admission boundary for callers; message text carries the specific rejected detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicationErrorKind {
    /// Malformed content or fixed-file path.
    Path,
    /// Invalid runtime or bundle identity.
    Identity,
    /// Content bytes do not match their URL digest.
    Digest,
    /// Extension and declared media type differ.
    Media,
    /// A bounded input or generation limit was exceeded.
    Limit,
    /// Missing or duplicate manifest reference.
    Manifest,
    /// Disposable cache identity/accounting failed.
    Cache,
    /// Malformed content negotiation.
    Encoding,
    /// Invalid compressed representation.
    Representation,
    /// Invalid canonical shell origin.
    Origin,
    /// Operating-system nonce generation failed.
    Randomness,
    /// Inline import-map serialization failed.
    Serialization,
    /// An admitted HTTP header could not be encoded.
    Header,
}
impl PublicationError {
    /// The precise admission boundary that refused the operation.
    pub fn kind(&self) -> PublicationErrorKind {
        self.kind
    }
}
pub(crate) fn refused(kind: PublicationErrorKind, detail: impl Into<String>) -> PublicationError {
    PublicationError {
        kind,
        detail: detail.into(),
    }
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn asset_path(path: &str) -> Result<(&str, &str), PublicationError> {
    let (hash, ext) = path.split_once('.').ok_or_else(|| {
        refused(
            PublicationErrorKind::Path,
            "expected lowercase SHA-256 and allowed extension",
        )
    })?;
    if !valid_hash(hash) || mime(ext).is_none() {
        return Err(refused(
            PublicationErrorKind::Path,
            "expected lowercase SHA-256 and allowed extension",
        ));
    }
    Ok((hash, ext))
}
fn mime(ext: &str) -> Option<&'static str> {
    match ext {
        "js" => Some("text/javascript; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        "json" => Some("application/json; charset=utf-8"),
        "svg" => Some("image/svg+xml"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "woff" => Some("font/woff"),
        "woff2" => Some("font/woff2"),
        "txt" => Some("text/plain; charset=utf-8"),
        _ => None,
    }
}
/// Verified cold input, consumed by admission; it cannot be cloned to retain uncharged payloads.
///
/// ```compile_fail
/// use erp_server::assets::AssetSource;
/// fn duplicate(source: AssetSource) { let _spare = source.clone(); }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct AssetSource {
    path: String,
    mime: String,
    bytes: Arc<[u8]>,
    gzip: Option<Arc<[u8]>>,
    brotli: Option<Arc<[u8]>>,
}
impl AssetSource {
    /// Admit encoded forms after verifying exact bounded decoding and rejecting trailing data.
    /// # Errors
    /// Invalid identity, media, digest, size or encoded representation.
    pub fn with_encodings(
        path: &str,
        media_type: &str,
        bytes: Vec<u8>,
        gzip: Vec<u8>,
        brotli: Vec<u8>,
    ) -> Result<Self, PublicationError> {
        let mut source = Self::new(path, media_type, bytes)?;
        verify::encoded(&gzip, &brotli, &source.bytes)?;
        source.gzip = Some(gzip.into());
        source.brotli = Some(brotli.into());
        Ok(source)
    }
    /// Check content identity, extension, media type and the per-asset admission bound.
    /// # Errors
    /// Invalid path, mismatched digest/media or bytes exceeding 16 MiB.
    pub fn new(path: &str, media_type: &str, bytes: Vec<u8>) -> Result<Self, PublicationError> {
        let (hash, ext) = asset_path(path)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(refused(PublicationErrorKind::Limit, "asset exceeds 16 MiB"));
        }
        if mime(ext) != Some(media_type) {
            return Err(refused(
                PublicationErrorKind::Media,
                "asset media type does not match its extension",
            ));
        }
        if hex(&Sha256::digest(&bytes)) != hash {
            return Err(refused(
                PublicationErrorKind::Digest,
                "asset digest mismatch",
            ));
        }
        Ok(Self {
            path: path.into(),
            mime: media_type.into(),
            bytes: bytes.into(),
            gzip: None,
            brotli: None,
        })
    }
}
/// An unserved tenant generation assembled by the host. All paths and references are admitted
/// by `build`; possession of bytes alone never registers a route.
#[derive(Debug)]
#[non_exhaustive]
pub struct AssetGeneration {
    runtime: String,
    key: String,
    assets: BTreeMap<String, AssetSource>,
    total_bytes: usize,
    imports: BTreeMap<String, String>,
    fixed: BTreeMap<String, String>,
}
impl AssetGeneration {
    /// Begin a tenant-selected committed runtime generation.
    /// # Errors
    /// Empty/oversized runtime identity or invalid generation hash.
    pub fn new(runtime: &str, key: String) -> Result<Self, PublicationError> {
        if runtime.is_empty()
            || runtime.len() > 128
            || runtime.chars().any(char::is_control)
            || !valid_hash(&key)
        {
            return Err(refused(
                PublicationErrorKind::Identity,
                "invalid runtime or generation identity",
            ));
        }
        Ok(Self {
            runtime: runtime.into(),
            key: key.into_boxed_str().into_string(),
            assets: BTreeMap::new(),
            total_bytes: 0,
            imports: BTreeMap::new(),
            fixed: BTreeMap::new(),
        })
    }
    /// Add already digest-verified bytes. A repeated path must carry identical bytes and media.
    /// # Errors
    /// Conflicting path, count or aggregate-size admission failure.
    pub fn asset(mut self, source: AssetSource) -> Result<Self, PublicationError> {
        if let Some(old) = self.assets.get(&source.path) {
            if old.bytes != source.bytes
                || old.mime != source.mime
                || old.gzip != source.gzip
                || old.brotli != source.brotli
            {
                return Err(refused(
                    PublicationErrorKind::Path,
                    "conflicting asset path",
                ));
            }
            return Ok(self);
        }
        let total = self
            .total_bytes
            .checked_add(source.bytes.len())
            .ok_or_else(|| refused(PublicationErrorKind::Limit, "asset byte admission overflow"))?;
        if self.assets.len() >= 40_001 || total > 128 * 1024 * 1024 {
            return Err(refused(
                PublicationErrorKind::Limit,
                "asset generation exceeds bounded admission",
            ));
        }
        self.total_bytes = total;
        self.assets.insert(source.path.clone(), source);
        Ok(self)
    }
    /// Declare a module or compiled-template import key and its local content-addressed URL.
    /// # Errors
    /// Invalid specifier/URL, duplicate import or count limit.
    pub fn import(mut self, specifier: &str, url: &str) -> Result<Self, PublicationError> {
        local_url(url)?;
        if specifier.is_empty()
            || specifier.starts_with(['.', '/'])
            || specifier.ends_with('/')
            || specifier
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || specifier.len() > 256
            || !specifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@/._-:".contains(&b))
        {
            return Err(refused(
                PublicationErrorKind::Manifest,
                "invalid bare import specifier",
            ));
        }
        if self.imports.len() >= 40_001
            || self.imports.insert(specifier.into(), url.into()).is_some()
        {
            return Err(refused(
                PublicationErrorKind::Manifest,
                "duplicate or excessive import specifier",
            ));
        }
        Ok(self)
    }
    /// Declare an exact module static path. The host supplies only installed-addon declarations.
    /// # Errors
    /// Invalid path/URL, duplicate declaration or count limit.
    pub fn static_file(mut self, path: &str, url: &str) -> Result<Self, PublicationError> {
        local_url(url)?;
        if !fixed_path(path) {
            return Err(refused(
                PublicationErrorKind::Path,
                "invalid declared module static path",
            ));
        }
        if self.fixed.len() >= 20_000 || self.fixed.insert(path.into(), url.into()).is_some() {
            return Err(refused(
                PublicationErrorKind::Manifest,
                "duplicate or excessive static declaration",
            ));
        }
        Ok(self)
    }
    /// Complete admission, resolving every reference to this tenant's verified bytes.
    /// # Errors
    /// Missing blob references, conflicting declarations or resident admission refusal.
    pub fn build(self, store: &Arc<AssetStore>) -> Result<PublishedAssets, PublicationError> {
        for url in self.imports.values().chain(self.fixed.values()) {
            let path = local_url(url)?;
            if !self.assets.contains_key(path) {
                return Err(refused(
                    PublicationErrorKind::Manifest,
                    "unreferenced asset URL in manifest",
                ));
            }
        }
        let metadata = metadata_cost(
            std::iter::once(self.runtime.as_str())
                .chain(std::iter::once(self.key.as_str()))
                .chain(self.assets.keys().map(String::as_str))
                .chain(
                    self.imports
                        .iter()
                        .flat_map(|(key, value)| [key.as_str(), value.as_str()]),
                )
                .chain(
                    self.fixed
                        .iter()
                        .flat_map(|(key, value)| [key.as_str(), value.as_str()]),
                ),
        )?;
        let (assets, reservation) = store.admit(self.assets, metadata)?;
        Ok(PublishedAssets {
            runtime: self.runtime,
            key: self.key,
            assets,
            _metadata: reservation,
            store: Arc::clone(store),
            imports: self.imports,
            fixed: self.fixed,
        })
    }
}
// Charge owned strings and conservative map/node bookkeeping per string. Sources normalize
// externally supplied String capacities before residence; this is logical admission, not RSS.
fn metadata_cost<'a>(mut strings: impl Iterator<Item = &'a str>) -> Result<u64, PublicationError> {
    strings.try_fold(512u64, |total, value| {
        total
            .checked_add(value.len() as u64)
            .and_then(|n| n.checked_add(256))
            .ok_or_else(|| {
                refused(
                    PublicationErrorKind::Limit,
                    "assets.memory_bytes admission limit exceeded",
                )
            })
    })
}
fn local_url(url: &str) -> Result<&str, PublicationError> {
    let path = url.strip_prefix("/erp/assets/").ok_or_else(|| {
        refused(
            PublicationErrorKind::Path,
            "expected local content-addressed asset URL",
        )
    })?;
    asset_path(path)?;
    Ok(path)
}
fn fixed_path(path: &str) -> bool {
    if path.len() > 1024 {
        return false;
    }
    let mut parts = path.split('/');
    parts.next() == Some("")
        && parts.next().is_some_and(|m| {
            !m.is_empty() && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        && parts.next() == Some("static")
        && {
            let rest: Vec<_> = parts.collect();
            !rest.is_empty()
                && rest.iter().all(|s| {
                    !s.is_empty()
                        && *s != "."
                        && *s != ".."
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                })
        }
}
/// A verified immutable snapshot selected from exactly one database's manifests. It performs no
/// assembly, filesystem I/O or database writes on requests.
#[derive(Debug)]
#[non_exhaustive]
pub struct PublishedAssets {
    runtime: String,
    key: String,
    assets: BTreeMap<String, Arc<store::ResidentAsset>>,
    store: Arc<AssetStore>,
    imports: BTreeMap<String, String>,
    fixed: BTreeMap<String, String>,
    _metadata: store::Reservation,
}
impl PublishedAssets {
    pub(crate) fn check_store(&self, expected: &Arc<AssetStore>) -> Result<(), PublicationError> {
        if !Arc::ptr_eq(&self.store, expected) {
            return Err(refused(
                PublicationErrorKind::Limit,
                "asset publication uses a different resident store",
            ));
        }
        Ok(())
    }
    /// The admitted runtime ABI identity.
    pub fn runtime(&self) -> &str {
        &self.runtime
    }
    /// Committed content generation.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// Exact declaration lookup. An unknown module/path never falls through to disk.
    pub fn fixed_path(&self, path: &str) -> Option<&str> {
        self.fixed.get(path).map(String::as_str)
    }
    /// Import-map JSON, escaped for an inline nonced script element.
    /// # Errors
    /// Serialization failure.
    pub fn import_map(&self) -> Result<String, PublicationError> {
        serde_json::to_string(&serde_json::json!({"imports":self.imports}))
            .map(|s| {
                s.replace('<', "\\u003c")
                    .replace('>', "\\u003e")
                    .replace('&', "\\u0026")
                    .replace('\u{2028}', "\\u2028")
                    .replace('\u{2029}', "\\u2029")
            })
            .map_err(|e| refused(PublicationErrorKind::Serialization, e.to_string()))
    }
    /// Serve admitted bytes with encoding negotiation and representation-specific validators.
    /// # Errors
    /// Malformed asset path or encoding header, or poisoned cache accounting.
    pub fn response(
        &self,
        path: &str,
        if_none_match: Option<&str>,
        accept_encoding: Option<&str>,
    ) -> Result<Option<axum::response::Response>, PublicationError> {
        asset_path(path)?;
        let Some(asset) = self.assets.get(path) else {
            return Ok(None);
        };
        self.store.touch(&asset.key)?;
        Ok(Some(asset.response(if_none_match, accept_encoding)?))
    }
}

/// Per-response import-map markup and matching shell policy; never cache this value.
#[derive(Debug)]
#[non_exhaustive]
pub struct ShellAssets {
    nonce: String,
    html: String,
    csp: String,
}
impl ShellAssets {
    /// Fresh OS-random nonce, used only by this response's import map.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    /// Escaped inline import map. All executable modules remain external.
    pub fn import_map_html(&self) -> &str {
        &self.html
    }
    /// Exact shell CSP, including only the configured websocket origin.
    pub fn csp(&self) -> &str {
        &self.csp
    }
}
impl PublishedAssets {
    /// Render the only nonced inline script and its policy, using 128 bits of OS randomness.
    /// # Errors
    /// Invalid canonical origin, randomness failure or JSON serialization failure.
    pub fn shell(&self, origin: &str) -> Result<ShellAssets, PublicationError> {
        use base64::Engine as _;
        if origin.len() > 4096 {
            return Err(refused(
                PublicationErrorKind::Origin,
                "invalid canonical shell origin",
            ));
        }
        let parsed = url::Url::parse(origin).map_err(|_| {
            refused(
                PublicationErrorKind::Origin,
                "invalid canonical shell origin",
            )
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(refused(
                PublicationErrorKind::Origin,
                "invalid canonical shell origin",
            ));
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| {
            refused(
                PublicationErrorKind::Randomness,
                "OS randomness unavailable for import-map nonce",
            )
        })?;
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let authority = &parsed[url::Position::BeforeHost..url::Position::AfterPort];
        let csp = format!(
            "default-src 'self'; script-src 'self' 'nonce-{nonce}'; style-src-elem 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self' wss://{authority}; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
        );
        let html = format!(
            "<script type=\"importmap\" nonce=\"{nonce}\">{}</script>",
            self.import_map()?
        );
        Ok(ShellAssets { nonce, html, csp })
    }
}

impl PublicationError {
    pub(crate) fn runtime_mismatch() -> Self {
        refused(PublicationErrorKind::Identity, "runtime mismatch")
    }
}

#[cfg(test)]
mod release_encoding_tests {
    use super::*;

    #[allow(clippy::unwrap_used, reason = "tests")]
    fn release(dir: &Path, js: &[u8], css: &[u8]) -> std::path::PathBuf {
        let mut entries = Vec::new();
        for (name, extension, media, bytes) in [
            ("client.js", "js", "text/javascript; charset=utf-8", js),
            ("client.css", "css", "text/css; charset=utf-8", css),
        ] {
            let hash = hex(&Sha256::digest(bytes));
            let path = format!("client.{hash}.{extension}");
            std::fs::write(dir.join(&path), bytes).unwrap();
            entries.push(serde_json::json!({"name": name, "path": path, "sha256": hash, "content_type": media}));
        }
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&serde_json::json!({"schema": 1, "assets": entries})).unwrap(),
        )
        .unwrap();
        manifest
    }

    #[allow(clippy::unwrap_used, reason = "tests")]
    async fn body(response: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "tests")]
    async fn release_bundle_serves_br_gzip_and_identity_that_decode_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let js = b"export const value = 'release client';\n".repeat(200);
        let css = b"body { color: #123456; margin: 0; }\n".repeat(200);
        let manifest = release(dir.path(), &js, &css);
        let store = AssetStore::new(4_000_000).unwrap();
        let bundle = AssetBundle::load(&manifest, &store).unwrap();
        for (name, identity) in [("client.js", &js), ("client.css", &css)] {
            let path = bundle.path(name).unwrap().to_owned();
            for (accept, expected) in [
                (Some("br, gzip"), Some("br")),
                (Some("gzip"), Some("gzip")),
                (None, None),
            ] {
                let response = bundle.response(&path, None, accept).unwrap().unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                assert_eq!(
                    response.headers().get(axum::http::header::VARY).unwrap(),
                    "Accept-Encoding"
                );
                let encoding = response
                    .headers()
                    .get(axum::http::header::CONTENT_ENCODING)
                    .map(|value| value.to_str().unwrap().to_owned());
                assert_eq!(encoding.as_deref(), expected, "{name} with {accept:?}");
                let bytes = body(response).await;
                let decoded = match expected {
                    Some("br") => {
                        let mut out = Vec::new();
                        brotli::Decompressor::new(bytes.as_slice(), 4096)
                            .read_to_end(&mut out)
                            .unwrap();
                        assert!(bytes.len() < identity.len(), "{name} br is smaller");
                        out
                    }
                    Some("gzip") => {
                        let mut out = Vec::new();
                        flate2::read::GzDecoder::new(bytes.as_slice())
                            .read_to_end(&mut out)
                            .unwrap();
                        assert!(bytes.len() < identity.len(), "{name} gzip is smaller");
                        out
                    }
                    _ => bytes,
                };
                assert_eq!(
                    &decoded, identity,
                    "{name} with {accept:?} decodes to the file"
                );
            }
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "tests")]
    fn release_encoded_forms_are_charged_and_refused_as_a_whole() {
        let dir = tempfile::tempdir().unwrap();
        let js = b"export const value = 'release client';\n".repeat(200);
        let css = b"body { color: #123456; margin: 0; }\n".repeat(200);
        let manifest = release(dir.path(), &js, &css);
        let probe = AssetStore::new(4_000_000).unwrap();
        let bundle = AssetBundle::load(&manifest, &probe).unwrap();
        let used = probe.used_bytes();
        let identity = u64::try_from(js.len() + css.len()).unwrap();
        // Identity, gzip and Brotli payloads are all charged, plus metadata.
        assert!(used > identity, "encoded forms are charged");
        drop(bundle);
        let small = AssetStore::new(used - 1).unwrap();
        let error = match AssetBundle::load(&manifest, &small) {
            Ok(_) => panic!("identity-sized store must refuse encoded release forms"),
            Err(error) => error,
        };
        match error {
            AssetError::Admission(error) => {
                assert_eq!(error.kind(), PublicationErrorKind::Limit);
                assert_eq!(
                    error.to_string(),
                    "asset publication refused: assets.memory_bytes admission limit exceeded"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(small.used_bytes(), 0);
    }
}
