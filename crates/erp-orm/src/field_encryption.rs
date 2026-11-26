// SPDX-License-Identifier: MIT OR Apache-2.0
//! Encryption of field values at rest (`#[field(char, encrypted = true)]`).
//!
//! A stored `char` or `text` field declared `encrypted` keeps its value in a `bytea` column as an
//! envelope sealed with XChaCha20-Poly1305 (RustCrypto `chacha20poly1305`, no cryptography of our
//! own). The cache, `read`, `write` and every surface above them see the clear text; the database,
//! its dumps, replicas and any role reading the table see only the envelope.
//!
//! Envelope, in this order: the magic bytes `EF`, the format version (`1`), the length of the key
//! id, the key id, a random 24-byte nonce (one per sealed value: a new nonce on every write, so
//! two equal values never produce the same bytes), then the ciphertext and its 16-byte tag. The
//! associated data binds the envelope to its model and field and to its own header, so a value
//! copied into another column, or whose key id was edited, fails to open. Binding to the record
//! id is not done yet: a value moved to another row *of the same column* opens there (the
//! `INSERT` draws its ids in the statement, after the values are sealed).
//!
//! Keys come from the node configuration (`field_key_file` of `server.toml`), never from the
//! database, and are never logged ([`FieldKeys`] has no key bytes in its `Debug`). The first key
//! of the file seals; every key of the file opens. Rotation: put a new key first and keep the old
//! ones; values are sealed with the new key as they are written, and the old keys stay until no
//! envelope names them (a re-encryption job is a follow-up).
//!
//! The process installs its keys once ([`install`]) before it builds an ORM. Building an ORM
//! whose registry declares an encrypted field without installed keys is refused
//! ([`require_keys`]), as is sealing or opening a value.
//!
//! An envelope is opaque to SQL: conditions, ordering, grouping and aggregates on an encrypted
//! field are refused with an error naming the field, never answered with a result computed on
//! ciphertext. A field declared `blind_index = true` also keeps a keyed hash of its value in a
//! companion column, which equality conditions and typed keys use ([`blind`]).

use std::fmt;
use std::sync::{Arc, OnceLock};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use erp_fields::{Cell, Error, ErrorKind, Result};
use erp_registry::{FieldSpec, ModelSpec, Registry, TableObjectKind};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

mod blind;
mod sync;

pub use blind::BLIND_INDEX_LABEL;
pub(crate) use blind::{Blind, key_columns};
pub(crate) use sync::ClearColumn;

/// The first two bytes of every envelope.
const MAGIC: &[u8; 2] = b"EF";
/// The envelope format this build writes and reads.
const VERSION: u8 = 1;
/// Length of a key in bytes (256-bit keys).
const KEY_LEN: usize = 32;
/// Length of the XChaCha20 nonce.
const NONCE_LEN: usize = 24;
/// Length of the Poly1305 tag.
const TAG_LEN: usize = 16;
/// Longest key id.
const MAX_KEY_ID_LEN: usize = 32;
/// Most keys one key file may hold (one sealing key and the older ones still opening values).
const MAX_KEYS: usize = 16;
/// Domain separation of the associated data.
const AAD_LABEL: &[u8] = b"erpforall.field.v1";

/// One key of the key file.
#[derive(Clone)]
struct FieldKey {
    id: String,
    cipher: XChaCha20Poly1305,
    /// The key bytes, kept (wiped on drop) to derive the blind index keys.
    bytes: Zeroizing<[u8; KEY_LEN]>,
}

/// The field keys of this node: the first one seals, all of them open.
#[derive(Clone)]
pub struct FieldKeys {
    keys: Vec<FieldKey>,
    /// SHA-256 over the key ids and key bytes, to tell two key sets apart without keeping the
    /// bytes ([`install`] accepts the same set twice and refuses another one).
    fingerprint: [u8; 32],
}

impl fmt::Debug for FieldKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldKeys")
            .field("ids", &self.ids().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn key_file_error(message: impl fmt::Display) -> Error {
    Error::programming(format!("field_key_file: {message}"))
}

fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_KEY_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl FieldKeys {
    /// Parse the text of a key file: one key per line, `<id> <base64 of 32 bytes>`, the
    /// sealing key first. Blank lines and lines starting with `#` are skipped. A key id is 1 to
    /// 32 letters, digits, `.`, `_` or `-`; ids are unique; at most 16 keys.
    ///
    /// A key is generated with `openssl rand -base64 32`.
    ///
    /// # Errors
    ///
    /// A `Programming` error naming the line (never its content) when the text is not such a
    /// file or holds no key.
    pub fn parse(text: &str) -> Result<Self> {
        let mut keys: Vec<FieldKey> = Vec::new();
        let mut digest = Sha256::new();
        for (number, line) in text.lines().enumerate().map(|(i, l)| (i + 1, l.trim())) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(id), Some(encoded), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(key_file_error(format!(
                    "line {number} is not `<key id> <base64 key>`"
                )));
            };
            if !valid_key_id(id) {
                return Err(key_file_error(format!(
                    "line {number}: a key id is 1 to {MAX_KEY_ID_LEN} letters, digits, '.', '_' \
                     or '-'"
                )));
            }
            if keys.iter().any(|k| k.id == id) {
                return Err(key_file_error(format!(
                    "line {number}: key id '{id}' is listed twice"
                )));
            }
            let bytes =
                Zeroizing::new(erp_fields::base64::decode_strict(encoded).map_err(|_| {
                    key_file_error(format!("line {number}: the key is not valid base64"))
                })?);
            if bytes.len() != KEY_LEN {
                return Err(key_file_error(format!(
                    "line {number}: a key is {KEY_LEN} bytes (base64 of 32 random bytes, e.g. \
                     `openssl rand -base64 32`)"
                )));
            }
            if keys.len() == MAX_KEYS {
                return Err(key_file_error(format!("more than {MAX_KEYS} keys")));
            }
            let cipher = XChaCha20Poly1305::new_from_slice(&bytes)
                .map_err(|_| key_file_error(format!("line {number}: unusable key")))?;
            let mut kept = Zeroizing::new([0u8; KEY_LEN]);
            kept.copy_from_slice(&bytes);
            digest.update((id.len() as u64).to_le_bytes());
            digest.update(id.as_bytes());
            digest.update(&bytes[..]);
            keys.push(FieldKey {
                id: id.to_owned(),
                cipher,
                bytes: kept,
            });
        }
        if keys.is_empty() {
            return Err(key_file_error("the file holds no key"));
        }
        Ok(Self {
            keys,
            fingerprint: digest.finalize().into(),
        })
    }

    /// SHA-256 over the key ids and key bytes: equal for the same key file content, whatever its
    /// comments and blank lines. It reveals nothing usable about the keys; node settings compare
    /// it (keyed) so every node of a database runs with the same key set.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// The key ids, the sealing key first.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|k| k.id.as_str())
    }

    /// The id of the key new values are sealed with.
    pub fn sealing_id(&self) -> &str {
        &self.keys[0].id
    }

    /// The id of the key blind indexes are derived from: the last key of the file (the oldest
    /// one). Putting a new sealing key first leaves it in place, so a rotation does not change
    /// the blind indexes; removing the last line does, and the next module upgrade recomputes
    /// them.
    pub fn index_id(&self) -> &str {
        &self.index_key().id
    }

    fn index_key(&self) -> &FieldKey {
        // `parse` refuses a file without keys
        &self.keys[self.keys.len() - 1]
    }

    /// Seal `clear` for `model.field` with the sealing key and a fresh random nonce.
    ///
    /// # Errors
    ///
    /// `Internal` when the operating system's random source fails or the cipher refuses the
    /// input (a value longer than the cipher's limit of 256 GiB).
    pub fn seal(&self, model: &str, field: &str, clear: &[u8]) -> Result<Vec<u8>> {
        let key = &self.keys[0];
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| {
            Error::internal("the operating system's random source failed while sealing a value")
        })?;
        let header = header(&key.id);
        let aad = associated_data(model, field, &header);
        let sealed = key
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: clear,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::internal(format!("could not seal a value of {model}.{field}")))?;
        let mut out = Vec::with_capacity(header.len() + NONCE_LEN + sealed.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open an envelope of `model.field`.
    ///
    /// # Errors
    ///
    /// A `Database` error naming the field (never the value) when the bytes are not an envelope
    /// of this format, name a key this node does not have, or fail authentication (a wrong key,
    /// a value of another field, or tampered bytes).
    pub fn open(&self, model: &str, field: &str, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let fail = |why: &str| {
            Error::new(
                ErrorKind::Database,
                format!("the stored value of the encrypted field {model}.{field} {why}"),
            )
        };
        let (key_id, rest) = parse_header(envelope).ok_or_else(|| {
            fail(
                "is not an encrypted value (it was written without the ORM, or before the \
                 field was encrypted and not converted)",
            )
        })?;
        let header_len = envelope.len() - rest.len();
        let key = self.keys.iter().find(|k| k.id == key_id).ok_or_else(|| {
            fail(&format!(
                "is sealed with the key '{key_id}', which is not in this node's field_key_file"
            ))
        })?;
        if rest.len() < NONCE_LEN + TAG_LEN {
            return Err(fail("is truncated"));
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| fail("has a malformed nonce"))?;
        let aad = associated_data(model, field, &envelope[..header_len]);
        key.cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| {
                fail(&format!(
                    "does not open with the key '{key_id}' (another key with the same id, a \
                     value of another field, or altered bytes)"
                ))
            })
    }

    /// Seal a cache cell of `model.field`: text becomes its envelope, NULL stays NULL.
    pub(crate) fn seal_cell(&self, model: &str, field: &str, cell: Cell) -> Result<Cell> {
        match cell {
            Cell::Null => Ok(Cell::Null),
            Cell::Str(text) => Ok(Cell::Bytes(
                self.seal(model, field, text.as_bytes())?.into(),
            )),
            _ => Err(Error::invariant(format!(
                "the cache value of the encrypted field {model}.{field} is not text"
            ))),
        }
    }

    /// Open a fetched cell of `model.field`: an envelope becomes its text, NULL stays NULL.
    pub(crate) fn open_cell(&self, model: &str, field: &str, cell: Cell) -> Result<Cell> {
        match cell {
            Cell::Null => Ok(Cell::Null),
            Cell::Bytes(bytes) => {
                let clear = self.open(model, field, &bytes)?;
                let text = std::str::from_utf8(&clear).map_err(|_| {
                    Error::new(
                        ErrorKind::Database,
                        format!(
                            "the stored value of the encrypted field {model}.{field} is not \
                             UTF-8 text once opened"
                        ),
                    )
                })?;
                Ok(Cell::Str(text.into()))
            }
            _ => Err(Error::invariant(format!(
                "the column of the encrypted field {model}.{field} did not decode as bytes"
            ))),
        }
    }
}

/// `EF`, the version, the key id length and the key id.
fn header(key_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + 2 + key_id.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    // valid_key_id bounds the length to 32
    out.push(u8::try_from(key_id.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(key_id.as_bytes());
    out
}

/// The key id of an envelope and what follows its header; `None` when the bytes do not start
/// with a header of this version.
fn parse_header(envelope: &[u8]) -> Option<(&str, &[u8])> {
    let rest = envelope.strip_prefix(MAGIC.as_slice())?;
    let (&version, rest) = rest.split_first()?;
    if version != VERSION {
        return None;
    }
    let (&len, rest) = rest.split_first()?;
    let len = usize::from(len);
    if len == 0 || len > MAX_KEY_ID_LEN || rest.len() < len {
        return None;
    }
    let (id, rest) = rest.split_at(len);
    let id = std::str::from_utf8(id).ok().filter(|id| valid_key_id(id))?;
    Some((id, rest))
}

/// The associated data: the label, the model, the field and the envelope header, each
/// length-prefixed so no two different triples produce the same bytes.
fn associated_data(model: &str, field: &str, header: &[u8]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(AAD_LABEL.len() + model.len() + field.len() + header.len() + 24);
    for part in [AAD_LABEL, model.as_bytes(), field.as_bytes(), header] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

/// The model and field a stored encrypted value belongs to: what its associated data names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sealed {
    model: &'static str,
    field: &'static str,
}

impl Sealed {
    /// `Some` for a field whose column holds envelopes (stored and encrypted).
    pub(crate) fn of(spec: &ModelSpec, field: &FieldSpec) -> Option<Self> {
        (field.encrypted && field.has_column()).then_some(Self {
            model: spec.name,
            field: field.name,
        })
    }

    /// Seal one cache cell for the column.
    pub(crate) fn seal(self, cell: Cell) -> Result<Cell> {
        keys_for(self.model, self.field)?.seal_cell(self.model, self.field, cell)
    }

    /// Seal the cells of one column parameter.
    pub(crate) fn seal_all(self, cells: Vec<Cell>) -> Result<Vec<Cell>> {
        let keys = keys_for(self.model, self.field)?;
        cells
            .into_iter()
            .map(|cell| keys.seal_cell(self.model, self.field, cell))
            .collect()
    }

    /// Open one fetched cell into its cache value.
    pub(crate) fn open(self, cell: Cell) -> Result<Cell> {
        keys_for(self.model, self.field)?.open_cell(self.model, self.field, cell)
    }
}

static INSTALLED: OnceLock<Arc<FieldKeys>> = OnceLock::new();

/// Install the field keys of this process (from `field_key_file`), before any ORM is built.
/// Installing the same key set again is a no-op.
///
/// # Errors
///
/// `Programming` when another key set is already installed: a process has one key set.
pub fn install(keys: FieldKeys) -> Result<()> {
    let fingerprint = keys.fingerprint;
    let installed = INSTALLED.get_or_init(|| Arc::new(keys));
    let diff = installed
        .fingerprint
        .iter()
        .zip(&fingerprint)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if diff != 0 {
        return Err(Error::programming(
            "another set of field keys is already installed in this process",
        ));
    }
    // conditions on blind-indexed fields hash their values with these keys
    erp_sql::blind::install(blind::hash_installed);
    Ok(())
}

/// The installed field keys, if any.
pub fn installed() -> Option<&'static Arc<FieldKeys>> {
    INSTALLED.get()
}

fn no_keys(model: &str, field: &str) -> Error {
    Error::programming(format!(
        "the field {model}.{field} is encrypted, but this process has no field keys: set \
         field_key_file in server.toml (see the deployment guide, \"Encrypted fields\")"
    ))
}

/// The installed keys, or the error naming `model.field` when there are none.
pub(crate) fn keys_for(model: &str, field: &str) -> Result<&'static FieldKeys> {
    installed()
        .map(AsRef::as_ref)
        .ok_or_else(|| no_keys(model, field))
}

/// Refuse a registry that declares an encrypted field when `keys` is `None`: a node that cannot
/// open the values must not start.
///
/// # Errors
///
/// `Programming`, naming the first encrypted field.
pub fn require_keys(registry: &Registry, keys: Option<&FieldKeys>) -> Result<()> {
    if keys.is_some() {
        return Ok(());
    }
    match encrypted_fields(registry).next() {
        Some((model, field)) => Err(no_keys(model.name, field.name)),
        None => Ok(()),
    }
}

/// The ORM's checks of a registry with encrypted fields: the keys are installed
/// ([`require_keys`]), and no typed key, index or exclusion constraint names an encrypted field
/// (its column holds envelopes with random nonces: equal values never compare equal, so a unique
/// key would accept duplicates and an index would serve nothing). A typed key may name a
/// blind-indexed field: it is built on the field's companion column, which holds equal hashes for
/// equal values. Keys and indexes written as SQL text are not parsed: an addon must not name an
/// encrypted column (or a companion column) in them. No field may be named like the companion
/// column of a blind-indexed field.
///
/// # Errors
///
/// `Programming`, naming the field and the table object.
pub fn validate(registry: &Registry) -> Result<()> {
    require_keys(registry, installed().map(AsRef::as_ref))?;
    for model in registry.models() {
        for object in &model.table_objects {
            let (named, blind_allowed): (Vec<&str>, bool) = match &object.kind {
                TableObjectKind::Key(key) => (key.fields.to_vec(), true),
                TableObjectKind::TypedIndex(index) => (
                    index.fields.iter().chain(index.include).copied().collect(),
                    false,
                ),
                TableObjectKind::Exclusion(exclusion) => (exclusion.equal.to_vec(), false),
                TableObjectKind::TemporalKey(key) => (key.fields.to_vec(), false),
                _ => continue,
            };
            let refused = named
                .iter()
                .filter_map(|name| model.field(name))
                .find(|field| {
                    field.encrypted && !(blind_allowed && field.blind_index_column().is_some())
                });
            if let Some(field) = refused {
                let hint = if field.blind_index_column().is_some() {
                    " (the companion column of a blind-indexed field is indexed by the ORM, and a \
                     typed key may name the field)"
                } else {
                    " (declare `blind_index = true` for a typed key on it)"
                };
                return Err(Error::programming(format!(
                    "{}.{}: the table object '{}' names the encrypted field '{}'; a key, index or \
                     exclusion constraint cannot use an encrypted field{hint}",
                    model.name, field.name, object.name, field.name
                )));
            }
        }
        for field in &model.fields {
            if let Some(column) = field.blind_index_column() {
                if column.len() > 63 {
                    return Err(Error::programming(format!(
                        "{}.{}: blind index companion column '{column}' exceeds PostgreSQL's \
                         63-byte identifier limit",
                        model.name, field.name
                    )));
                }
                if model.field(&column).is_some() {
                    return Err(Error::programming(format!(
                        "{}.{column}: this field has the name of the blind index column of the \
                         encrypted field '{}'",
                        model.name, field.name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The stored encrypted fields of `registry` (those with a column of their own).
pub fn encrypted_fields(registry: &Registry) -> impl Iterator<Item = (&ModelSpec, &FieldSpec)> {
    registry.models().iter().flat_map(|model| {
        model
            .fields
            .iter()
            .filter(|f| f.encrypted && f.has_column())
            .map(move |f| (model, f))
    })
}

#[cfg(test)]
mod tests;
