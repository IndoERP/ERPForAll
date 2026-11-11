//! Native addon migration declarations: the element type of the
//! [`MIGRATIONS`] link-time slice that `#[erp_sdk::migration]` fills.
//!
//! Only the declaration lives here. Selecting, ordering and running the migrations of an upgrade
//! is `erp_modules::migrations` (L8). The declaration sits below `erp-sdk` (L10) so that an addon
//! that depends only on `erp-sdk` can register a migration: `erp-sdk` reaches it through
//! `erp_sdk::__private`, and `erp-modules` reads the same slice. Design note:
//! "Round 2 fixes", MUST 1.

use erp_orm::{BoxFuture, Transaction};
use std::fmt;

/// A `major.minor.patch` version: a module's installed or manifest version in its semver form
/// , and the version a migration step targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct SemverVersion {
    /// Major part.
    pub major: u32,
    /// Minor part.
    pub minor: u32,
    /// Patch part.
    pub patch: u32,
}

impl SemverVersion {
    /// The version `major.minor.patch`.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Whether this is `0.0.0`, the folder of the scripts that run on every upgrade.
    pub const fn is_zero(&self) -> bool {
        self.major == 0 && self.minor == 0 && self.patch == 0
    }
}

impl fmt::Display for SemverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The three migration stages.
///
/// Not `#[non_exhaustive]`: the stages are fixed by the ADR and every consumer (the upgrade
/// runner's stage split and order) matches them exhaustively, so a new stage must break each one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum MigrationStage {
    /// `pre`: before the module's schema is updated and its data files are loaded.
    Pre,
    /// `post`: after the module's schema is updated and its data files are loaded.
    Post,
    /// `end`: after every module of the update operation was loaded.
    End,
}

impl MigrationStage {
    /// The stage's name: `pre`, `post` or `end`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
            Self::End => "end",
        }
    }
}

impl fmt::Display for MigrationStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A native migration function: runs in the operation's transaction and receives the module's
/// previously installed version.
///
/// `#[erp_sdk::migration]` accepts an `async fn(&mut Transaction, &SemverVersion) -> Result<()>`
/// and generates a function of this type for it; a plain `fn` must already have this type.
///
/// The transaction contract: the function works inside the operation's transaction and never
/// ends it. It must not send transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`,
/// `RELEASE`, `PREPARE TRANSACTION`, ...) through `execute`, `query` or `batch`: a commit would
/// persist the upgrade's earlier work even when a later step fails. Native code is trusted, so
/// this is not scanned as SQL scripts are; the module system checks after each step that the
/// operation's transaction is still the one it began and fails the operation when it is not,
/// which detects the violation but cannot undo a commit that already happened.
pub type MigrationFn =
    for<'a> fn(&'a mut Transaction, &'a SemverVersion) -> BoxFuture<'a, erp_fields::Result<()>>;

/// One native migration of one addon, as `#[erp_sdk::migration]` registers it in [`MIGRATIONS`].
///
/// The version string is checked when the module system is built
/// (`erp_modules::system::ModuleSystemBuilder::build`), together with the addon name and
/// duplicates.
#[derive(Debug)]
pub struct MigrationImpl {
    addon: &'static str,
    version: &'static str,
    stage: MigrationStage,
    name: &'static str,
    run: MigrationFn,
    location: Option<(&'static str, u32)>,
}

impl MigrationImpl {
    /// A migration of `addon` to `version` (a migration folder name such as `1.1` or
    /// `20.0.1.1`) in `stage`; `name` orders it among the steps of the same version
    /// and stage.
    pub const fn new(
        addon: &'static str,
        version: &'static str,
        stage: MigrationStage,
        name: &'static str,
        run: MigrationFn,
    ) -> Self {
        Self {
            addon,
            version,
            stage,
            name,
            run,
            location: None,
        }
    }

    /// The source location of the implementation (`file!()`, `line!()`), named in errors.
    pub const fn at(mut self, file: &'static str, line: u32) -> Self {
        self.location = Some((file, line));
        self
    }

    /// The addon owning this migration.
    pub fn addon(&self) -> &'static str {
        self.addon
    }

    /// The version string, as declared.
    pub fn version(&self) -> &'static str {
        self.version
    }

    /// The stage.
    pub fn stage(&self) -> MigrationStage {
        self.stage
    }

    /// The name that orders it within its version and stage.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The function.
    pub fn run(&self) -> MigrationFn {
        self.run
    }

    /// `file:line`, or a note that [`MigrationImpl::at`] was not called.
    pub fn location(&self) -> String {
        match self.location {
            Some((file, line)) => format!("{file}:{line}"),
            None => format!(
                "unknown location (MigrationImpl::at was not called for {}'s {})",
                self.addon, self.name
            ),
        }
    }
}

/// Native migrations linked into the binary.
#[linkme::distributed_slice]
pub static MIGRATIONS: [MigrationImpl];
