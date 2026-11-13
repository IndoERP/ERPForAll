//! The data-loading extension point (amended): the
//! module system reads an addon's data and demo files and hands their typed operations to the
//! [`LoadSession`] of a [`DataLoader`], which implements ([`crate::data_loader`]).
//!
//! The loader receives an iterator of `erp-data` operations, never a whole-file `Vec`, so the
//! reader can become streaming without changing this contract.
//!
//! The runner checks the loader's work: a parse error the iterator yielded fails the operation
//! even when the loader returned `Ok` ([`crate::Error::DataParse`]), and so does a loader that
//! returned before the iterator was exhausted ([`crate::Error::DataLoaderStoppedEarly`]).

use std::sync::Arc;

use erp_data::load::{AddonFiles, DataNotice};
use erp_data::{ConvertMode, DataOperation, ParseError};
use erp_orm::{BaseInstallGrant, BoxFuture, Transaction};

use crate::manifest::ModuleName;

/// The operations of one data file, in file order; a parse error ends the file.
pub type Operations<'o> = dyn Iterator<Item = Result<DataOperation, ParseError>> + Send + 'o;

/// Which manifest list a file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataKind {
    /// `data` (always loaded).
    Data,
    /// `demo` (loaded when the module gets demo data).
    Demo,
}

impl DataKind {
    /// The manifest key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Demo => "demo",
        }
    }
}

/// One data file being loaded. Built by the module system; loaders read it through the getters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFile {
    module: ModuleName,
    path: String,
    kind: DataKind,
    mode: ConvertMode,
}

impl DataFile {
    /// A file of `module` at manifest path `path`.
    ///
    /// ```
    /// use erp_data::ConvertMode;
    /// use erp_modules::{DataFile, DataKind, ModuleName};
    /// let file = DataFile::new(ModuleName::new("x")?, "demo/d.xml".into(), DataKind::Demo, ConvertMode::Init);
    /// assert!(file.noupdate());
    /// assert_eq!((file.module().as_str(), file.path()), ("x", "demo/d.xml"));
    /// # Ok::<(), erp_modules::Error>(())
    /// ```
    pub fn new(module: ModuleName, path: String, kind: DataKind, mode: ConvertMode) -> Self {
        Self {
            module,
            path,
            kind,
            mode,
        }
    }

    /// The module the file belongs to.
    pub fn module(&self) -> &ModuleName {
        &self.module
    }

    /// The path as listed in the manifest.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// `data` or `demo`.
    pub fn kind(&self) -> DataKind {
        self.kind
    }

    /// `init` for install and reinit, `update` for upgrade.
    pub fn mode(&self) -> ConvertMode {
        self.mode
    }

    /// Derived from [`DataFile::kind`], so the two never disagree.
    pub fn noupdate(&self) -> bool {
        self.kind == DataKind::Demo
    }
}

/// The context a [`DataLoader`] opens a session with: what the module system knows and the
/// loader needs. Built by the module system; `'a` is the session's borrow of the module system.
#[derive(Clone)]
#[non_exhaustive]
pub struct SessionContext<'a> {
    files: Arc<dyn AddonFiles>,
    series: &'static str,
    base_install_grant: Option<&'a BaseInstallGrant>,
}

impl<'a> SessionContext<'a> {
    pub(crate) fn new(files: Arc<dyn AddonFiles>, series: &'static str) -> Self {
        Self {
            files,
            series,
            base_install_grant: None,
        }
    }

    /// Lend the session the base-install grant of the module system's ORM lineage.
    pub(crate) fn with_base_install_grant(mut self, grant: &'a BaseInstallGrant) -> Self {
        self.base_install_grant = Some(grant);
        self
    }

    /// The base-install grant of the module-install executor, which the session needs to mark
    /// the transaction while `base`'s data files load ([`Transaction::begin_base_install`]).
    /// The module system always lends it; `None` only in a context built without it, where
    /// loading `base` fails with [`crate::Error::BaseInstallGrantMissing`].
    pub fn base_install_grant(&self) -> Option<&'a BaseInstallGrant> {
        self.base_install_grant
    }

    /// Reads the files of the addons of the operation (bounded, no links followed).
    pub fn files(&self) -> &Arc<dyn AddonFiles> {
        &self.files
    }

    pub fn series(&self) -> &'static str {
        self.series
    }
}

impl std::fmt::Debug for SessionContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContext")
            .field("series", &self.series)
            .field("base_install_grant", &self.base_install_grant.is_some())
            .finish_non_exhaustive()
    }
}

/// Loads the operations of data files into the database, and removes the records of uninstalled
/// modules (implemented by [`crate::data_loader::ErpDataLoader`]; as amended by
/// ).
///
/// One [`LoadSession`] serves one module operation: the module system opens it before the first
/// module of the run and drops it after the last, so the loader can keep what the whole run
/// needs (the external ids it loaded, a cache) without state in the shared `Arc<dyn DataLoader>`.
///
/// Called inside the operation's transaction, on the stage registry of the module, as the
/// superuser. An error aborts the whole operation; a parse error of the file
/// ([`crate::Error::DataParse`]) and ORM errors ([`crate::Error::Orm`]) convert with `?`. The
/// loader must consume every operation: returning early, or dropping a parse error, fails the
/// operation anyway (see the module documentation).
///
/// A loader with nothing to keep between files implements [`FileLoader`] instead; every
/// `FileLoader` is a `DataLoader`.
pub trait DataLoader: Send + Sync {
    /// Open the session of one module operation. It may borrow the loader.
    fn session<'a>(&'a self, context: SessionContext<'a>) -> Box<dyn LoadSession + 'a>;

    /// Called in the operation's transaction, after the modules' uninstall hooks, on the registry
    /// that still has the modules (their tables and columns exist), before the module system
    /// removes their tables, columns and remaining external ids. When it returns `Ok` while
    /// external ids of `modules` still point at models that stay installed, the uninstall fails
    /// ([`crate::Error::PurgeIncomplete`]): deleting only the external ids would orphan the
    /// records.
    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<Vec<DataNotice>>>;
}

/// A point of a [`LoadSession`] to go back to ([`LoadSession::mark`], [`LoadSession::rollback_to`]).
/// It is opaque to the module system: the loader that made it puts in whatever identifies its own
/// state and takes it out again with [`SessionMark::into_inner`].
pub struct SessionMark(Box<dyn std::any::Any + Send>);

impl SessionMark {
    /// A mark holding `state`.
    pub fn new<T: std::any::Any + Send>(state: T) -> Self {
        Self(Box::new(state))
    }

    /// The state the mark was made with, or the mark back when it holds a `T` of another type
    /// (a mark of another loader).
    ///
    /// # Errors
    /// The mark itself, when it does not hold a `T`.
    pub fn into_inner<T: std::any::Any>(self) -> std::result::Result<T, Self> {
        match self.0.downcast::<T>() {
            Ok(state) => Ok(*state),
            Err(other) => Err(Self(other)),
        }
    }
}

impl std::fmt::Debug for SessionMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionMark").finish_non_exhaustive()
    }
}

/// The session of one module operation (see [`DataLoader::session`]).
///
/// The iterator's own lifetime `'o` is separate from the borrow `'a`: the reader may borrow the
/// file content for longer than the call.
pub trait LoadSession: Send {
    /// Load one file.
    fn load<'a, 'o: 'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, crate::Result<()>>;

    /// The modules whose data this operation installs or upgrades, in load order, before the
    /// first one loads. Once per session.
    ///
    /// # Errors
    /// [`crate::Error::OperationOrder`] when the session has begun an operation already, for an
    /// implementation that checks the order (see [`LoadSession::finish_module`]).
    fn begin_modules(&mut self, modules: &[ModuleName]) -> crate::Result<()>;

    /// The data of `module` is loaded (its demo data included). Returns the names (without the
    /// module) of the external ids the operation loaded for it
    /// ([`LoadedNames::Tracked`]), or [`LoadedNames::Untracked`] for a loader that does not
    /// track them. After an upgrade the module system validates the `noupdate` views among them
    /// (`ir.ui.view._validate_module_views`), and refuses the upgrade when
    /// the registry has that check and the loader tracks nothing.
    ///
    /// The order of [`LoadSession::begin_modules`] and `finish_module` is checked by each
    /// implementation, not by the module system (review T-9): [`crate::ErpDataLoader`]'s session
    /// checks it; a [`FileLoader`]'s session keeps no state across files and accepts any order.
    ///
    /// # Errors
    /// [`crate::Error::OperationOrder`], for an implementation that checks the order, when the
    /// call does not follow [`LoadSession::begin_modules`] for a module of it, or comes twice for
    /// one module.
    fn finish_module(&mut self, module: &ModuleName) -> crate::Result<LoadedNames>;

    /// The current point; the module system takes it before the demo savepoint.
    fn mark(&mut self) -> SessionMark;

    /// Back to `mark` after the savepoint was rolled back: what the session learned since (the
    /// external ids it loaded, its cache) is forgotten.
    ///
    /// # Errors
    /// `Programming` when `mark` was taken from another session.
    fn rollback_to(&mut self, mark: SessionMark) -> crate::Result<()>;

    /// After every module of the run was installed, upgraded or reinitialised and before the
    /// uninstall hooks: the end-of-run cleanup (`_process_end`).
    /// `updated` are the modules of the run, in order.
    fn process_end<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        updated: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<()>>;

    /// The notices collected since the last call (returned in
    /// [`crate::OperationOutcome::data_notices`]).
    fn take_notices(&mut self) -> Vec<DataNotice>;
}

/// What [`LoadSession::finish_module`] reports of the external ids a module loaded (review T-3:
/// one enum, not an `Option` used as a capability flag).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum LoadedNames {
    /// The names (without the module) the operation loaded for the module.
    /// Ordering is not part of this contract; [`crate::ErpDataLoader`] sorts them bytewise.
    Tracked(Vec<String>),
    /// The loader does not track them.
    Untracked,
}

/// A loader without state across files: [`FileLoader::load`] gets each file on its own.
/// Every `FileLoader` is a [`DataLoader`] whose session calls it per file and has nothing to
/// clean up. For tests and for loaders that only record or forward.
pub trait FileLoader: Send + Sync {
    /// Load one file.
    fn load<'a, 'o: 'a>(
        &'a self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, crate::Result<()>>;

    /// See [`DataLoader::purge`].
    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<Vec<DataNotice>>>;
}

impl<T: FileLoader> DataLoader for T {
    fn session<'a>(&'a self, _context: SessionContext<'a>) -> Box<dyn LoadSession + 'a> {
        Box::new(FileSession { loader: self })
    }

    fn purge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        modules: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<Vec<DataNotice>>> {
        FileLoader::purge(self, tx, modules)
    }
}

/// The session of a [`FileLoader`].
struct FileSession<'a, T: FileLoader> {
    loader: &'a T,
}

impl<T: FileLoader> LoadSession for FileSession<'_, T> {
    fn load<'a, 'o: 'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        file: &'a DataFile,
        operations: &'a mut Operations<'o>,
    ) -> BoxFuture<'a, crate::Result<()>> {
        self.loader.load(tx, file, operations)
    }

    /// A file loader keeps no state across files: it does not check the order.
    fn begin_modules(&mut self, _modules: &[ModuleName]) -> crate::Result<()> {
        Ok(())
    }

    /// A file loader keeps no external ids across files.
    fn finish_module(&mut self, _module: &ModuleName) -> crate::Result<LoadedNames> {
        Ok(LoadedNames::Untracked)
    }

    fn mark(&mut self) -> SessionMark {
        SessionMark::new(())
    }

    fn rollback_to(&mut self, _mark: SessionMark) -> crate::Result<()> {
        Ok(())
    }

    fn process_end<'a>(
        &'a mut self,
        _tx: &'a mut Transaction,
        _updated: &'a [ModuleName],
    ) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn take_notices(&mut self) -> Vec<DataNotice> {
        Vec::new()
    }
}

/// The iterator handed to a loader: remembers the first parse error it yielded, so the runner can
/// fail even when the loader dropped it.
pub(crate) struct Checked<'i, 'o> {
    inner: &'i mut Operations<'o>,
    first_error: Option<ParseError>,
}

impl<'i, 'o> Checked<'i, 'o> {
    pub(crate) fn new(inner: &'i mut Operations<'o>) -> Self {
        Self {
            inner,
            first_error: None,
        }
    }

    /// After the loader returned `Ok`: the first parse error it saw, else whether operations were
    /// left unread.
    pub(crate) fn finish(mut self) -> std::result::Result<(), LoaderVerdict> {
        if let Some(error) = self.first_error.take() {
            return Err(LoaderVerdict::Parse(error));
        }
        match self.inner.next() {
            None => Ok(()),
            Some(Err(error)) => Err(LoaderVerdict::Parse(error)),
            Some(Ok(_)) => Err(LoaderVerdict::StoppedEarly),
        }
    }
}

/// Why a loader's `Ok` is not accepted.
pub(crate) enum LoaderVerdict {
    /// The file has a parse error the loader did not return.
    Parse(ParseError),
    /// The loader returned before the end of the file.
    StoppedEarly,
}

impl Iterator for Checked<'_, '_> {
    type Item = Result<DataOperation, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;
        if let Err(error) = &item
            && self.first_error.is_none()
        {
            self.first_error = Some(error.clone());
        }
        Some(item)
    }
}
