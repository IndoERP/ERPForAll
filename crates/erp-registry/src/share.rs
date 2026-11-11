//! Registry sharing: one immutable `Arc<Registry>` per (addon set,
//! overlay), and the current registry of each database.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use erp_fields::{Error, ErrorKind, Result};
use rustc_hash::FxHashSet;

use crate::compose::RegistryBuilder;
use crate::decl::AddonDescriptor;
use crate::drop::ManualFieldPolicy;
use crate::overlay::Overlay;
use crate::owned::InternAccount;
use crate::spec::Registry;

/// One addon of a cache key: its name and dependencies (the load order depends on both).
type AddonKey = (&'static str, &'static [&'static str], bool);

/// How often [`RegistryCache::get`] looks for its entry again when failed builds of other callers
/// keep removing it while it waits (each retry needs another caller's build to fail).
const MAX_ATTEMPTS: usize = 64;

/// A cache key. The hash mixes the addon list with the overlay's cached content hash, so a key is
/// cheap to build; `Eq` still compares everything. The overlay is a cheap clone (its declarations
/// sit behind an `Arc`).
#[derive(Debug, Clone)]
struct Key {
    hash: u64,
    addons: Vec<AddonKey>,
    overlay: Overlay,
    /// A strict and a dropping build of the same overlay never share an entry: one may fail
    /// where the other drops fields.
    policy: ManualFieldPolicy,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
            && self.policy == other.policy
            && self.addons == other.addons
            && self.overlay == other.overlay
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

/// One cache entry: filled by the first caller, which builds under the entry's own lock.
type Cell = Arc<Mutex<Option<Arc<Registry>>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every value behind these locks is replaced whole (a registry is inserted only once fully
    // built), so a panic in another thread leaves it consistent; recovering the guard is safe.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Composed registries keyed by addon set and overlay. Databases with the same
/// installed addons (names and dependencies) and the same overlay (typically: none) get the same
/// `Arc`; any difference in the overlay gives a separate registry, so custom declarations never
/// leak between databases.
///
/// Registries are obtained through [`Databases`], which charges each database's newly interned
/// bytes to that database's own [`InternAccount`]; the cache has no public lookup of its own, so
/// nothing can compose a database's overlay against a fresh account and step around its quota
/// (review M4).
///
/// Composing a registry is CPU work of a few milliseconds to a few hundred; an async server calls
/// [`Databases::load`] and [`Databases::set_overlay`] from `tokio::task::spawn_blocking` or an
/// equivalent, not from a runtime worker thread.
#[derive(Debug)]
pub struct RegistryCache {
    linked: bool,
    hasher: RandomState,
    entries: Mutex<HashMap<Key, Cell>>,
}

impl Default for RegistryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryCache {
    /// A cache composing with the declarations linked into the binary.
    pub fn new() -> Self {
        Self {
            linked: true,
            hasher: RandomState::new(),
            entries: Mutex::default(),
        }
    }

    /// A cache composing without the linked declarations (see [`RegistryBuilder::linked`]).
    pub fn unlinked() -> Self {
        Self {
            linked: false,
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn key(&self, addons: &[&'static AddonDescriptor], overlay: &Overlay) -> Result<Key> {
        self.key_with(addons, overlay, ManualFieldPolicy::Strict)
    }

    fn key_with(
        &self,
        addons: &[&'static AddonDescriptor],
        overlay: &Overlay,
        policy: ManualFieldPolicy,
    ) -> Result<Key> {
        let mut list: Vec<AddonKey> = addons
            .iter()
            .map(|a| (a.name, a.depends, a.is_exact_only()))
            .collect();
        list.sort_unstable();
        list.dedup();
        if let Some(pair) = list.windows(2).find(|w| w[0].0 == w[1].0) {
            let message = if pair[0].1 != pair[1].1 {
                format!(
                    "addon '{}' is listed twice with different dependencies ({:?} and {:?})",
                    pair[0].0, pair[0].1, pair[1].1
                )
            } else {
                format!(
                    "addon '{}' is listed twice with different exact_only policies",
                    pair[0].0
                )
            };
            return Err(Error::programming(message));
        }
        let mut hasher = self.hasher.build_hasher();
        list.hash(&mut hasher);
        overlay.hash(&mut hasher);
        policy.hash(&mut hasher);
        Ok(Key {
            hash: hasher.finish(),
            addons: list,
            overlay: overlay.clone(),
            policy,
        })
    }

    /// The registry of `addons` with `overlay`, composed on first use. The cache lock is held only
    /// to find the entry; the build runs under the entry's own lock, so concurrent requests for the
    /// same new key wait for one build while other keys are served. New interned bytes are
    /// charged to the overlay's [`InternAccount`]; callers attach the database's account first
    /// (crate-private for that reason, see the type docs).
    ///
    /// # Errors
    ///
    /// `Programming` when one addon name comes with two different dependency lists; the errors of
    /// [`RegistryBuilder::build`]. Nothing is cached on error.
    #[cfg(test)]
    pub(crate) fn get(
        &self,
        addons: &[&'static AddonDescriptor],
        overlay: &Overlay,
    ) -> Result<Arc<Registry>> {
        self.get_with(addons, overlay, ManualFieldPolicy::Strict)
    }

    /// [`RegistryCache::get`] with a policy for broken custom fields (part of the key).
    pub(crate) fn get_with(
        &self,
        addons: &[&'static AddonDescriptor],
        overlay: &Overlay,
        policy: ManualFieldPolicy,
    ) -> Result<Arc<Registry>> {
        let key = self.key_with(addons, overlay, policy)?;
        self.get_key(&key, || {
            RegistryBuilder::new(addons.iter().copied())
                .linked(self.linked)
                .overlay(overlay)
                .manual_fields(policy)
                .build()
        })
    }

    fn get_key(&self, key: &Key, build: impl Fn() -> Result<Registry>) -> Result<Arc<Registry>> {
        self.get_key_with(key, build, |_| {})
    }

    /// [`RegistryCache::get_key`], calling `before_fill` with each entry found before trying to
    /// fill it (tests use it to detach the entry the way a failing build of another caller does).
    ///
    /// # Errors
    ///
    /// `Concurrency` (retryable) when the entry was detached [`MAX_ATTEMPTS`] times in a row:
    /// other callers' builds of the same key kept failing, which is contention, not a bug of
    /// this caller (review N4); the errors of `build`.
    fn get_key_with(
        &self,
        key: &Key,
        build: impl Fn() -> Result<Registry>,
        before_fill: impl Fn(&Cell),
    ) -> Result<Arc<Registry>> {
        for _ in 0..MAX_ATTEMPTS {
            let cell = self.cell(key);
            before_fill(&cell);
            if let Some(registry) = self.fill(key, &cell, &build)? {
                return Ok(registry);
            }
        }
        Err(Error::new(
            ErrorKind::Concurrency,
            format!(
                "the registry cache entry was removed {MAX_ATTEMPTS} times by failed builds of \
                 other callers while this caller waited for it; try again"
            ),
        ))
    }

    /// The entry of `key`, inserted empty when missing.
    fn cell(&self, key: &Key) -> Cell {
        let mut entries = lock(&self.entries);
        match entries.get(key) {
            Some(cell) => Arc::clone(cell),
            None => Arc::clone(entries.entry(key.clone()).or_default()),
        }
    }

    /// The registry in `cell`, built into it when empty. `Ok(None)` when the cell was detached
    /// from the cache while this caller waited for its lock (another caller's build failed and
    /// removed it): building into it would give a registry nobody else can find, so the caller
    /// looks the key up again.
    fn fill(
        &self,
        key: &Key,
        cell: &Cell,
        build: &impl Fn() -> Result<Registry>,
    ) -> Result<Option<Arc<Registry>>> {
        let mut slot = lock(cell);
        if let Some(found) = slot.as_ref() {
            return Ok(Some(Arc::clone(found)));
        }
        // lock order: cell, then the cache (`remove_if_empty` holds the cache and only tries the
        // cell, `drop_unused` locks only cells nobody else holds)
        let attached = lock(&self.entries)
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        if !attached {
            return Ok(None);
        }
        match build() {
            Ok(registry) => {
                let registry = Arc::new(registry);
                *slot = Some(Arc::clone(&registry));
                Ok(Some(registry))
            }
            Err(error) => {
                drop(slot);
                self.remove_if_empty(key, cell);
                Err(error)
            }
        }
    }

    /// Remove the entry of `key` when it is still `cell` and nothing was stored in it. A cell
    /// another caller is filling right now stays (that caller removes it if its build fails too).
    fn remove_if_empty(&self, key: &Key, cell: &Cell) {
        let mut entries = lock(&self.entries);
        let empty = entries.get(key).is_some_and(|current| {
            Arc::ptr_eq(current, cell)
                && match current.try_lock() {
                    Ok(slot) => slot.is_none(),
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_none(),
                    Err(TryLockError::WouldBlock) => false,
                }
        });
        if empty {
            entries.remove(key);
        }
    }

    /// Drop the entry of `key` when nobody but the cache holds its registry (a database moved to
    /// another overlay). Returns whether it was dropped.
    fn release(&self, key: &Key) -> bool {
        let mut entries = lock(&self.entries);
        let unused = entries.get(key).is_some_and(|cell| {
            Arc::strong_count(cell) == 1
                && lock(cell)
                    .as_ref()
                    .is_some_and(|registry| Arc::strong_count(registry) == 1)
        });
        if unused {
            entries.remove(key);
        }
        unused
    }

    /// Number of entries, filled or not (tests check that failed builds leave none behind).
    #[cfg(test)]
    fn entry_count(&self) -> usize {
        lock(&self.entries).len()
    }

    /// Number of cached registries (entries still being built are not counted).
    pub fn len(&self) -> usize {
        lock(&self.entries)
            .values()
            .filter(|cell| match cell.try_lock() {
                Ok(slot) => slot.is_some(),
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_some(),
                // being built by another caller: not cached yet
                Err(TryLockError::WouldBlock) => false,
            })
            .count()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop the registries nobody else holds; returns how many were dropped. Entries being built
    /// right now are kept.
    pub fn drop_unused(&self) -> usize {
        let mut entries = lock(&self.entries);
        let mut dropped = 0usize;
        entries.retain(|_, cell| {
            // a cell another caller holds is in use (being built or read)
            if Arc::strong_count(cell) > 1 {
                return true;
            }
            let keep = lock(cell)
                .as_ref()
                .is_some_and(|registry| Arc::strong_count(registry) > 1);
            if !keep {
                dropped = dropped.saturating_add(1);
            }
            keep
        });
        dropped
    }
}

#[derive(Debug)]
struct Loaded {
    addons: Vec<&'static AddonDescriptor>,
    overlay: Overlay,
    /// What the database's builds do with broken custom fields ([`Databases::load_with`]).
    policy: ManualFieldPolicy,
    registry: Arc<Registry>,
    /// The ticket of the [`Databases::set_overlay`] call applying an overlay to this database
    /// right now; a second call is refused while it is set (review N3). [`Databases::load`]
    /// keeps it when it replaces the entry.
    applying: Option<u64>,
}

/// Clears the `applying` ticket of a [`Databases::set_overlay`] call however the call ends:
/// returned, failed, or its future dropped part way.
struct Applying<'a> {
    loaded: &'a Mutex<HashMap<String, Loaded>>,
    db: &'a str,
    ticket: u64,
}

impl Drop for Applying<'_> {
    fn drop(&mut self) {
        // only this call's ticket: the database may have been unloaded and loaded again, and
        // another call may be applying to the new entry by now
        if let Some(current) = lock(self.loaded).get_mut(self.db)
            && current.applying == Some(self.ticket)
        {
            current.applying = None;
        }
    }
}

/// Releases the cache entry built for a [`Databases::set_overlay`] call when the call ends
/// without installing it, including when its future is dropped during `sync`. Declared before
/// the call's registry handle so it drops after it: `release` only drops an entry nobody else
/// holds.
struct ReleaseNew<'a> {
    cache: &'a RegistryCache,
    key: &'a Key,
    armed: bool,
}

impl Drop for ReleaseNew<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cache.release(self.key);
        }
    }
}

/// The current registry of each database, built through a shared [`RegistryCache`].
///
/// Each database has one [`InternAccount`] for the life of this value: [`Databases::load`] and
/// [`Databases::set_overlay`] charge the bytes its overlays newly intern to it, so one database
/// churning custom fields hits its own quota and never another database's registry builds.
///
/// A changed overlay is installed only through [`Databases::set_overlay`], which synchronizes the
/// database schema with the new registry first and swaps on success, so no request ever sees a
/// registry whose columns do not exist yet.
///
/// ```
/// use std::sync::Arc;
/// use erp_registry::{Databases, FieldDef, KindDef, Overlay, RegistryCache};
/// # fn block_on<F: std::future::Future>(f: F) -> F::Output {
/// #     let mut f = std::pin::pin!(f);
/// #     let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
/// #     loop { if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
/// # }
///
/// let dbs = Databases::new(RegistryCache::unlinked());
/// let a = dbs.load("db_a", &[], Overlay::new())?;
/// let b = dbs.load("db_b", &[], Overlay::new())?;
/// assert!(Arc::ptr_eq(&a, &b)); // same addons, same overlay: one registry
///
/// let mut custom = Overlay::new();
/// custom.add_manual_field("res.users", FieldDef::new("x_badge", KindDef::Char { size: None }))?;
/// // the closure synchronizes the schema (`erp_orm::Orm::sync_schema`) before the swap
/// let a2 = block_on(dbs.set_overlay("db_a", custom, |_registry| async { Ok(()) }))?;
/// assert!(a2.model("res.users").and_then(|m| m.field("x_badge")).is_some());
/// assert!(b.model("res.users").and_then(|m| m.field("x_badge")).is_none());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct Databases {
    cache: RegistryCache,
    loaded: Mutex<HashMap<String, Loaded>>,
    accounts: Mutex<HashMap<String, Arc<InternAccount>>>,
    /// Source of `Loaded::applying` tickets. A `u64` counter does not wrap in practice (one
    /// ticket per `set_overlay` call); tickets are only compared for equality.
    tickets: AtomicU64,
}

impl Databases {
    /// Databases sharing `cache`.
    pub fn new(cache: RegistryCache) -> Self {
        Self {
            cache,
            loaded: Mutex::default(),
            accounts: Mutex::default(),
            tickets: AtomicU64::new(0),
        }
    }

    /// The shared cache.
    pub fn cache(&self) -> &RegistryCache {
        &self.cache
    }

    /// The intern account of `db`, created with the default limit on first use (so a database
    /// that never had one gets [`InternAccount::new`] here, and keeps it).
    pub fn account_or_default(&self, db: &str) -> Arc<InternAccount> {
        Arc::clone(lock(&self.accounts).entry(db.to_owned()).or_default())
    }

    /// Give `db` an account with a lower limit ([`InternAccount::with_limit`]) before its first
    /// load.
    ///
    /// # Errors
    ///
    /// `Programming` when `db` has an account already (replacing it would reset what the
    /// database has used).
    pub fn set_account(&self, db: &str, account: Arc<InternAccount>) -> Result<()> {
        let mut accounts = lock(&self.accounts);
        if accounts.contains_key(db) {
            return Err(Error::programming(format!(
                "database '{db}' has an intern account already; set it before the first load"
            )));
        }
        accounts.insert(db.to_owned(), account);
        Ok(())
    }

    /// Load database `db` with its installed addons and overlay, as the database stands: the
    /// schema is expected to match (server start). A loaded database may be loaded again with
    /// the same overlay (e.g. after its addon set changed); a changed overlay goes through
    /// [`Databases::set_overlay`], which synchronizes the schema first. The overlay's bytes are
    /// charged to `db`'s account.
    ///
    /// # Errors
    ///
    /// `Programming` when `db` is loaded with another overlay (nothing is built then); the
    /// errors of the registry build (`Programming` for an addon listed twice with different
    /// dependencies, and those of [`RegistryBuilder::build`]). On error the database keeps its
    /// previous registry.
    pub fn load(
        &self,
        db: &str,
        addons: &[&'static AddonDescriptor],
        overlay: Overlay,
    ) -> Result<Arc<Registry>> {
        self.load_with(db, addons, overlay, ManualFieldPolicy::Strict)
    }

    /// [`Databases::load`] with a policy for broken custom fields. With
    /// [`ManualFieldPolicy::DropBroken`] (e.g. at server start after an addon upgrade) custom
    /// fields the build refuses for a reason of their own are left out and listed in the
    /// registry's [`Registry::dropped_fields`] instead of failing the database;
    /// the columns of dropped fields stay in the database. The policy is kept for the database:
    /// [`Databases::set_overlay`] builds with it too, until the next load.
    ///
    /// # Errors
    ///
    /// Those of [`Databases::load`], minus the custom-field refusals `DropBroken` drops.
    pub fn load_with(
        &self,
        db: &str,
        addons: &[&'static AddonDescriptor],
        overlay: Overlay,
        policy: ManualFieldPolicy,
    ) -> Result<Arc<Registry>> {
        self.refuse_other_overlay(db, &overlay)?;
        let overlay = overlay.with_account(self.account_or_default(db));
        let registry = self.cache.get_with(addons, &overlay, policy)?;
        let mut loaded = lock(&self.loaded);
        // checked again under the lock: a `set_overlay` may have finished while this built
        if let Some(current) = loaded.get(db)
            && current.overlay != overlay
        {
            return Err(Self::other_overlay(db));
        }
        // a `set_overlay` running on this database keeps its guard
        let applying = loaded.get(db).and_then(|current| current.applying);
        loaded.insert(
            db.to_owned(),
            Loaded {
                addons: addons.to_vec(),
                overlay,
                policy,
                registry: Arc::clone(&registry),
                applying,
            },
        );
        Ok(registry)
    }

    fn refuse_other_overlay(&self, db: &str, overlay: &Overlay) -> Result<()> {
        match lock(&self.loaded).get(db) {
            Some(current) if current.overlay != *overlay => Err(Self::other_overlay(db)),
            Some(_) | None => Ok(()),
        }
    }

    fn other_overlay(db: &str) -> Error {
        Error::programming(format!(
            "database '{db}' is loaded with another overlay; change it with \
             Databases::set_overlay, which synchronizes the schema first"
        ))
    }

    /// The current registry of `db`, if loaded.
    pub fn registry(&self, db: &str) -> Option<Arc<Registry>> {
        lock(&self.loaded).get(db).map(|l| Arc::clone(&l.registry))
    }

    /// The current overlay of `db`, if loaded (a cheap clone).
    pub fn overlay(&self, db: &str) -> Option<Overlay> {
        lock(&self.loaded).get(db).map(|l| l.overlay.clone())
    }

    /// Replace the overlay of a loaded database (custom models or fields changed): build the new
    /// registry, run `sync` with it — the schema synchronization, e.g. `erp_orm::Orm::sync_schema`
    /// on the database's pool, which must commit before it returns `Ok` — and only then install
    /// it. Other databases keep theirs. The previous registry's cache entry is dropped when no
    /// other database uses it. The build uses the policy for broken custom fields the database
    /// was loaded with ([`Databases::load_with`]). With [`ManualFieldPolicy::DropBroken`] the
    /// custom fields the current registry drops carry over, but a build that drops any other
    /// custom field (one the overlay adds, or one the change breaks) is refused with the error a
    /// strict build reports for that field, before `sync` runs.
    ///
    /// One overlay change at a time per database: while a call is applying an overlay, a second
    /// call for the same database is refused with a `Concurrency` error before it builds
    /// anything (review N3); other databases are not affected. Composing is CPU work: in an
    /// async server, build on a blocking thread (see [`RegistryCache`]). Schema synchronization
    /// is not only additive: it adds tables and columns, changes column types, backfills
    /// defaults and sets or drops `NOT NULL`. When [`Databases::load`] replaces the database's
    /// registry while `sync` runs (after an unload, or for a changed addon set), the schema has
    /// already been changed for this overlay but the other registry is installed: the call fails
    /// with a `Concurrency` error and the caller applies its overlay again (which synchronizes
    /// again, converging the schema with the overlay installed then).
    ///
    /// # Run it in a spawned task
    ///
    /// This future is **not cancel-safe**. Between the committed schema synchronization and the
    /// swap, requests still using the old registry can fail (e.g. on a new required column
    /// without a database default), and the schema epoch is per `erp_orm::Orm`, so the old
    /// `Orm`'s prepared statements are not refreshed ("Caveats recorded at
    /// acceptance"). Run it in its own spawned task (`tokio::spawn`), never inside a request
    /// future that a client disconnect or a timeout can drop. If the future is dropped anyway,
    /// the database keeps its previous registry and the next call may apply an overlay again.
    /// A per-database schema epoch and an expand/contract swap come with M3
    /// .
    ///
    /// # Errors
    ///
    /// - `Programming` when `db` is not loaded, or was unloaded by another caller during the
    ///   call;
    /// - `Concurrency` (retryable) when another call is applying an overlay to `db` (nothing
    ///   is built), or when [`Databases::load`] replaced the database's registry while `sync`
    ///   ran (its schema was synchronized with this overlay already);
    /// - the errors of the registry build (see [`Databases::load`]), the strict error of a custom
    ///   field the new registry drops and the current one does not, and the error of `sync`.
    ///
    /// On error the database keeps the registry and overlay it has at that moment, and the
    /// registry built for this call is dropped from the cache when nobody uses it.
    pub async fn set_overlay<F, Fut>(
        &self,
        db: &str,
        overlay: Overlay,
        sync: F,
    ) -> Result<Arc<Registry>>
    where
        F: FnOnce(Arc<Registry>) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let (addons, before, old_overlay, policy, ticket) = {
            let mut loaded = lock(&self.loaded);
            let current = loaded.get_mut(db).ok_or_else(|| {
                Error::programming(format!("database '{db}' has no loaded registry"))
            })?;
            if current.applying.is_some() {
                return Err(Error::new(
                    ErrorKind::Concurrency,
                    format!(
                        "an overlay is being applied to database '{db}' already; wait for it to \
                         finish and apply this one again"
                    ),
                ));
            }
            let ticket = self.tickets.fetch_add(1, Ordering::Relaxed);
            current.applying = Some(ticket);
            (
                current.addons.clone(),
                Arc::clone(&current.registry),
                current.overlay.clone(),
                current.policy,
                ticket,
            )
        };
        let _applying = Applying {
            loaded: &self.loaded,
            db,
            ticket,
        };
        let overlay = overlay.with_account(self.account_or_default(db));
        // both keys before anything changes, so nothing can fail after the swap (review M3)
        let old_key = self.cache.key_with(&addons, &old_overlay, policy)?;
        let new_key = self.cache.key_with(&addons, &overlay, policy)?;
        let mut release_new = ReleaseNew {
            cache: &self.cache,
            key: &new_key,
            armed: true,
        };
        let registry = self.cache.get_with(&addons, &overlay, policy)?;
        let fail = |registry: Arc<Registry>,
                    before: Arc<Registry>,
                    error: Error|
         -> Result<Arc<Registry>> {
            // `release_new` releases the entry when the call returns, after `registry` is gone
            drop(registry);
            drop(before);
            Err(error)
        };
        // an overlay change may not drop a field the database has (review item 1): only fields
        // the current registry drops already carry over
        if let Some(error) = newly_dropped(&before, &registry) {
            return fail(registry, before, error);
        }
        if let Err(error) = sync(Arc::clone(&registry)).await {
            return fail(registry, before, error);
        }
        let swapped = {
            let mut loaded = lock(&self.loaded);
            match loaded.get_mut(db) {
                Some(current) if Arc::ptr_eq(&current.registry, &before) => {
                    current.overlay = overlay;
                    current.registry = Arc::clone(&registry);
                    current.applying = None;
                    Ok(())
                }
                Some(_) => Err(Error::new(
                    ErrorKind::Concurrency,
                    format!(
                        "database '{db}' changed while its overlay was being applied (a load \
                         replaced its registry); its schema was already synchronized with the \
                         new overlay, apply it again"
                    ),
                )),
                None => Err(Error::programming(format!(
                    "database '{db}' was unloaded while its overlay was being applied"
                ))),
            }
        };
        if let Err(error) = swapped {
            let concurrent = error.kind() == ErrorKind::Concurrency;
            let result = fail(registry, before, error);
            if concurrent {
                // the winner could not release the registry this call still held
                self.cache.release(&old_key);
            }
            return result;
        }
        release_new.armed = false;
        if !Arc::ptr_eq(&before, &registry) {
            drop(before);
            self.cache.release(&old_key);
        }
        Ok(registry)
    }

    /// Forget database `db` (its registry stays cached until [`RegistryCache::drop_unused`]; its
    /// intern account stays, so a reload cannot reset its use).
    pub fn unload(&self, db: &str) -> bool {
        lock(&self.loaded).remove(db).is_some()
    }
}

/// The strict error of the first custom field `after` drops and `before` does not (a
/// [`ManualFieldPolicy::DropBroken`] database must not lose a field it has through an overlay
/// change); `None` when every field `after` drops was dropped by `before` already.
fn newly_dropped(before: &Registry, after: &Registry) -> Option<Error> {
    let carried: FxHashSet<(&str, &str)> = before
        .dropped_fields()
        .iter()
        .map(|d| (d.model(), d.field()))
        .collect();
    after
        .dropped_fields()
        .iter()
        .find(|d| !carried.contains(&(d.model(), d.field())))
        // composer refusals carry a kind and a message only (no source, SQLSTATE or context),
        // so this is the error a strict build returns for the field
        .map(|d| Error::new(d.error().kind(), d.error().message()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::sync::Barrier;

    use super::*;
    use crate::owned::budget_lock::shared;
    use crate::owned::{FieldDef, KindDef};

    fn broken() -> Overlay {
        let mut overlay = Overlay::new();
        overlay
            .add_manual_field("x_nowhere", FieldDef::new("x_a", KindDef::Integer))
            .unwrap();
        overlay
    }

    #[test]
    fn failed_builds_leave_no_entry() {
        let _budget = shared();
        let cache = RegistryCache::unlinked();
        for _ in 0..3 {
            let err = cache.get(&[], &broken()).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                err.message(),
                "Model 'x_nowhere' does not exist in registry."
            );
        }
        assert_eq!(cache.entry_count(), 0);
        cache.get(&[], &Overlay::new()).unwrap();
        assert_eq!((cache.entry_count(), cache.len()), (1, 1));
    }

    /// Many threads asking for one new key at once get one registry; failing keys leave nothing.
    #[test]
    fn concurrent_gets_build_once() {
        let _budget = shared();
        let cache = Arc::new(RegistryCache::unlinked());
        let barrier = Arc::new(Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    if i % 2 == 0 {
                        Some(cache.get(&[], &Overlay::new()).unwrap())
                    } else {
                        assert!(cache.get(&[], &broken()).is_err());
                        None
                    }
                })
            })
            .collect();
        let registries: Vec<Arc<Registry>> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(registries.len(), 8);
        assert!(registries.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
        assert_eq!(cache.entry_count(), 1);
    }

    /// Poll a future that never waits (the sync closures here are ready at once).
    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    fn custom() -> Overlay {
        let mut overlay = Overlay::new();
        overlay
            .add_manual_field(
                "res.users",
                FieldDef::new("x_share_test", KindDef::Char { size: None }),
            )
            .unwrap();
        overlay
    }

    /// `set_overlay` refuses a database that is not loaded.
    #[test]
    fn set_overlay_after_unload() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        dbs.load("db", &[], Overlay::new()).unwrap();
        assert!(dbs.unload("db"));
        let err =
            block_on(dbs.set_overlay("db", Overlay::new(), |_| async { Ok(()) })).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(err.message(), "database 'db' has no loaded registry");
    }

    /// Review item 2: the schema sync runs with the new registry before the swap; a failing sync
    /// keeps the old registry and overlay installed and leaves no cache entry behind.
    #[test]
    fn set_overlay_syncs_first_and_swaps_on_success() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        let before = dbs.load("db", &[], Overlay::new()).unwrap();
        let err = block_on(dbs.set_overlay("db", custom(), |registry| async move {
            // the sync sees the new registry while the database still has the old one
            assert!(
                registry
                    .model("res.users")
                    .unwrap()
                    .field("x_share_test")
                    .is_some()
            );
            Err(Error::new(erp_fields::ErrorKind::Database, "sync failed"))
        }))
        .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Database);
        assert_eq!(err.message(), "sync failed");
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &before));
        assert!(dbs.overlay("db").unwrap().is_empty());
        assert_eq!(
            dbs.cache().entry_count(),
            1,
            "the failed overlay's registry was dropped"
        );
        drop(before);

        let mut synced = None;
        let after = block_on(dbs.set_overlay("db", custom(), |registry| {
            synced = Some(Arc::clone(&registry));
            async { Ok(()) }
        }))
        .unwrap();
        assert!(Arc::ptr_eq(&after, synced.as_ref().unwrap()));
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &after));
        assert_eq!(dbs.overlay("db").unwrap(), custom());
        drop(synced);
        // the old registry was released with the swap
        assert_eq!(dbs.cache().entry_count(), 1);
    }

    /// Review item 6: the previous registry stays cached while another database uses it.
    #[test]
    fn set_overlay_keeps_a_shared_previous_registry() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        let plain = dbs.load("a", &[], Overlay::new()).unwrap();
        dbs.load("b", &[], Overlay::new()).unwrap();
        block_on(dbs.set_overlay("a", custom(), |_| async { Ok(()) })).unwrap();
        drop(plain);
        assert_eq!(
            dbs.cache().entry_count(),
            2,
            "'b' still uses the plain registry"
        );
    }

    /// Review item 4: a caller that waited on an entry another caller's failed build detached
    /// does not build into the orphan; it looks the key up again, so its registry is the cached
    /// one.
    #[test]
    fn a_detached_entry_is_not_filled() {
        let _budget = shared();
        let cache = RegistryCache::unlinked();
        let key = cache.key(&[], &Overlay::new()).unwrap();
        let build = || RegistryBuilder::new([]).linked(false).build();
        // this caller found the entry …
        let cell = cache.cell(&key);
        // … then the first caller's build failed and removed it
        cache.remove_if_empty(&key, &cell);
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.fill(&key, &cell, &build).unwrap().is_none());
        assert!(lock(&cell).is_none(), "nothing was built into the orphan");
        let first = cache.get_key(&key, build).unwrap();
        let again = cache.get(&[], &Overlay::new()).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(cache.entry_count(), 1);
    }

    /// Review item 4: failing and succeeding builds of the same key race; every success returns
    /// the one cached registry.
    #[test]
    fn failing_and_succeeding_builds_race_on_one_key() {
        let _budget = shared();
        for _ in 0..20 {
            let cache = Arc::new(RegistryCache::unlinked());
            let key = cache.key(&[], &Overlay::new()).unwrap();
            let barrier = Arc::new(Barrier::new(16));
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let (cache, key, barrier) =
                        (Arc::clone(&cache), key.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        cache.get_key(&key, || {
                            if i % 2 == 0 {
                                Err(Error::programming("injected build failure"))
                            } else {
                                RegistryBuilder::new([]).linked(false).build()
                            }
                        })
                    })
                })
                .collect();
            let mut ok = Vec::new();
            for handle in handles {
                match handle.join().unwrap() {
                    Ok(registry) => ok.push(registry),
                    Err(err) => assert_eq!(err.message(), "injected build failure"),
                }
            }
            let cached = cache.get(&[], &Overlay::new()).unwrap();
            assert!(ok.iter().all(|r| Arc::ptr_eq(r, &cached)));
            assert_eq!(cache.entry_count(), 1);
        }
    }

    /// Review M2: a loaded database changes its overlay only through `set_overlay` (which syncs
    /// the schema first); `load` with another overlay is refused and changes nothing. The same
    /// overlay reloads.
    #[test]
    fn load_refuses_a_different_overlay() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        let before = dbs.load("db", &[], Overlay::new()).unwrap();
        let err = dbs.load("db", &[], custom()).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "database 'db' is loaded with another overlay; change it with Databases::set_overlay, \
             which synchronizes the schema first"
        );
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &before));
        assert!(dbs.overlay("db").unwrap().is_empty());
        assert_eq!(
            dbs.cache().entry_count(),
            1,
            "nothing was built for the refused overlay"
        );
        let again = dbs.load("db", &[], Overlay::new()).unwrap();
        assert!(Arc::ptr_eq(&again, &before));
    }

    fn winner() -> Overlay {
        let mut winner = Overlay::new();
        winner
            .add_manual_field(
                "res.users",
                FieldDef::new("x_share_winner", KindDef::Integer),
            )
            .unwrap();
        winner
    }

    /// Review M3: a load that replaces the database's registry while this call syncs (here
    /// after an unload, with another overlay) makes this call fail with a retryable
    /// `Concurrency` error saying the schema was synchronized; the winner stays installed and
    /// the loser's registry is released.
    #[test]
    fn set_overlay_losing_a_race_is_a_concurrency_error() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        drop(dbs.load("db", &[], Overlay::new()).unwrap());
        let mut won = None;
        let err = block_on(dbs.set_overlay("db", custom(), |_| {
            assert!(dbs.unload("db"));
            won = Some(dbs.load("db", &[], winner()).unwrap());
            async { Ok(()) }
        }))
        .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Concurrency);
        assert_eq!(
            err.message(),
            "database 'db' changed while its overlay was being applied (a load replaced its \
             registry); its schema was already synchronized with the new overlay, apply it again"
        );
        let won = won.unwrap();
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &won));
        assert_eq!(dbs.overlay("db").unwrap(), winner());
        drop(won);
        assert_eq!(
            dbs.cache().entry_count(),
            1,
            "only the winner's registry is left"
        );
        // the reloaded entry is not marked as being applied
        block_on(dbs.set_overlay("db", custom(), |_| async { Ok(()) })).unwrap();
    }

    fn applying_refused(err: &Error) {
        assert_eq!(err.kind(), erp_fields::ErrorKind::Concurrency);
        assert_eq!(
            err.message(),
            "an overlay is being applied to database 'db' already; wait for it to finish and \
             apply this one again"
        );
    }

    /// Review N3: two `set_overlay` calls for one database on two threads. While the first
    /// one synchronizes, the second is refused with `Concurrency` before it builds anything,
    /// also after a `load` of the same overlay replaced the entry meanwhile; another database
    /// is not held up. The first call then installs its overlay and the database accepts the
    /// next call. Without the guard the second call builds and installs its overlay.
    #[test]
    fn a_second_set_overlay_for_one_database_is_refused_while_one_applies() {
        let _budget = shared();
        use std::sync::mpsc;

        let dbs = Arc::new(Databases::new(RegistryCache::unlinked()));
        drop(dbs.load("db", &[], Overlay::new()).unwrap());
        drop(dbs.load("other", &[], Overlay::new()).unwrap());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let first = {
            let dbs = Arc::clone(&dbs);
            std::thread::spawn(move || {
                block_on(dbs.set_overlay("db", custom(), move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    async { Ok(()) }
                }))
            })
        };
        entered_rx.recv().unwrap();
        // plain + the first call's registry
        assert_eq!(dbs.cache().entry_count(), 2);
        let second = std::thread::spawn({
            let dbs = Arc::clone(&dbs);
            move || block_on(dbs.set_overlay("db", winner(), |_| async { Ok(()) }))
        })
        .join()
        .unwrap();
        applying_refused(&second.unwrap_err());
        assert_eq!(dbs.cache().entry_count(), 2, "nothing was built for it");
        // a reload with the same overlay keeps the guard
        drop(dbs.load("db", &[], Overlay::new()).unwrap());
        applying_refused(
            &block_on(dbs.set_overlay("db", winner(), |_| async { Ok(()) })).unwrap_err(),
        );
        // another database is not held up
        block_on(dbs.set_overlay("other", winner(), |_| async { Ok(()) })).unwrap();

        release_tx.send(()).unwrap();
        let installed = first.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &installed));
        assert_eq!(dbs.overlay("db").unwrap(), custom());
        drop(installed);
        block_on(dbs.set_overlay("db", winner(), |_| async { Ok(()) })).unwrap();
        assert_eq!(dbs.overlay("db").unwrap(), winner());
    }

    /// Review N3: the guard is released however a call ends: a failing sync, and a future
    /// dropped while its sync is pending (the database keeps its registry).
    #[test]
    fn the_applying_guard_is_released_on_error_and_on_drop() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        let before = dbs.load("db", &[], Overlay::new()).unwrap();
        let err = block_on(dbs.set_overlay("db", custom(), |_| async {
            Err(Error::new(erp_fields::ErrorKind::Database, "sync failed"))
        }))
        .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Database);
        assert_eq!(err.message(), "sync failed");

        {
            let mut pending = std::pin::pin!(dbs.set_overlay("db", custom(), |_| async {
                std::future::pending::<()>().await;
                Ok(())
            }));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            applying_refused(
                &block_on(dbs.set_overlay("db", winner(), |_| async { Ok(()) })).unwrap_err(),
            );
        }
        assert!(Arc::ptr_eq(&dbs.registry("db").unwrap(), &before));
        assert_eq!(
            dbs.cache().entry_count(),
            1,
            "the dropped call's registry was released from the cache"
        );
        block_on(dbs.set_overlay("db", winner(), |_| async { Ok(()) })).unwrap();
        assert_eq!(dbs.overlay("db").unwrap(), winner());
    }

    /// Review N4: a caller whose entry keeps being detached by other callers' failed builds
    /// gives up after `MAX_ATTEMPTS` with a retryable `Concurrency` error (contention, not
    /// misuse), never builds into a detached entry and leaves nothing behind.
    #[test]
    fn exhausted_cache_retries_are_a_concurrency_error() {
        let _budget = shared();
        let cache = RegistryCache::unlinked();
        let key = cache.key(&[], &Overlay::new()).unwrap();
        let built = std::sync::atomic::AtomicUsize::new(0);
        let seen = std::sync::atomic::AtomicUsize::new(0);
        let err = cache
            .get_key_with(
                &key,
                || {
                    built.fetch_add(1, Ordering::Relaxed);
                    RegistryBuilder::new([]).linked(false).build()
                },
                // what another caller's failing build does while this one waits
                |cell| {
                    seen.fetch_add(1, Ordering::Relaxed);
                    cache.remove_if_empty(&key, cell);
                },
            )
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Concurrency);
        assert_eq!(
            err.message(),
            "the registry cache entry was removed 64 times by failed builds of other callers \
             while this caller waited for it; try again"
        );
        assert_eq!(seen.load(Ordering::Relaxed), MAX_ATTEMPTS);
        assert_eq!(built.load(Ordering::Relaxed), 0);
        assert_eq!(cache.entry_count(), 0);
    }

    /// Review M3: a database unloaded while its overlay syncs fails the call and releases the
    /// new registry.
    #[test]
    fn set_overlay_after_concurrent_unload_releases_the_new_registry() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        drop(dbs.load("db", &[], Overlay::new()).unwrap());
        let err = block_on(dbs.set_overlay("db", custom(), |_| {
            assert!(dbs.unload("db"));
            async { Ok(()) }
        }))
        .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "database 'db' was unloaded while its overlay was being applied"
        );
        assert_eq!(
            dbs.cache().drop_unused(),
            1,
            "only the plain registry was left"
        );
        assert_eq!(dbs.cache().entry_count(), 0);
    }

    /// Review item 1: each database is charged to its own account, which survives unload.
    #[test]
    fn accounts_are_per_database_and_fixed() {
        let _budget = shared();
        let dbs = Databases::new(RegistryCache::unlinked());
        let small = Arc::new(InternAccount::with_limit(1024).unwrap());
        dbs.set_account("a", Arc::clone(&small)).unwrap();
        let err = dbs
            .set_account("a", Arc::new(InternAccount::new()))
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "database 'a' has an intern account already; set it before the first load"
        );
        dbs.load("a", &[], Overlay::new()).unwrap();
        assert!(Arc::ptr_eq(dbs.overlay("a").unwrap().account(), &small));
        dbs.unload("a");
        assert!(Arc::ptr_eq(&dbs.account_or_default("a"), &small));
        assert!(!Arc::ptr_eq(&dbs.account_or_default("b"), &small));
    }
}

#[cfg(test)]
mod exact_numeric_policy_tests {
    use super::*;
    #[test]
    fn sharing_never_reuses_an_ordinary_policy_for_exact_admission() {
        static ORDINARY: AddonDescriptor = AddonDescriptor {
            name: "policy_cache",
            depends: &["base"],
            exact_only: false,
        };
        static EXACT: AddonDescriptor = AddonDescriptor {
            name: "policy_cache",
            depends: &["base"],
            exact_only: true,
        };
        let cache = RegistryCache::unlinked();
        let overlay = Overlay::new();
        let ordinary = cache.get(&[&ORDINARY], &overlay).unwrap();
        let exact = cache.get(&[&EXACT], &overlay).unwrap();
        assert!(!Arc::ptr_eq(&ordinary, &exact));
        assert!(Arc::ptr_eq(
            &exact,
            &cache.get(&[&EXACT], &overlay).unwrap()
        ));
        assert!(
            exact
                .addons()
                .iter()
                .find(|addon| addon.name == "policy_cache")
                .unwrap()
                .is_exact_only()
        );
    }
}
