//! `erp access` (Revision 2 S3): the access policy of a database, read and
//! repaired as the `System` principal.
//!
//! - `erp access report`: what the policy holds ([`erp_security::admin::AccessReport`]).
//! - `erp access disable-rule <module.name>`: switch off a rule
//!   ([`erp_security::admin::disable_rule`]), the recovery path for a rule that cannot be
//!   compiled.
//!
//! The connection credentials come from `DATABASE_URL`, the database from `-d`. The registry is
//! composed from the addons linked into this binary (`base` and the external-id model); composing
//! the registry of a given database (its installed modules and overlay) comes with.

use std::str::FromStr;
use std::sync::Arc;

use erp_orm::{Orm, SUPERUSER_ID};
// the report checks the capabilities the server declares: link the crates that declare them
use erp_registry::{Registry, RegistryBuilder};
use erp_rpc as _;
use erp_security::admin::{AccessReport, disable_rule};
use erp_security::loader::{check_registry, load_rows_from};

use crate::args::{AccessAction, AccessArgs};
use crate::error::{CliError, Result};

/// Run an `erp access` action and return what to print.
///
/// # Errors
///
/// [`CliError::InvalidInput`] for a connection string that does not parse or names no database;
/// [`CliError::Execution`] when the runtime cannot start; [`CliError::Access`] for the errors of
/// the registry, the database, the loader and the rule update.
pub fn execute(action: &AccessAction) -> Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))?;
    match action {
        AccessAction::Report(args) => runtime.block_on(report(args)),
        AccessAction::DisableRule(args) => runtime.block_on(disable(&args.db, &args.xmlid)),
    }
}

/// The environment variable holding the connection credentials (cli.md: credentials never come
/// from the command line).
pub const DATABASE_URL: &str = "DATABASE_URL";

/// The connection configuration: the credentials of [`DATABASE_URL`], the database of `-d`. The
/// connection string itself is never echoed (it may hold a password).
fn config(args: &AccessArgs) -> Result<tokio_postgres::Config> {
    connection_config(&args.database)
}

/// The connection configuration for `database`: the credentials come from [`DATABASE_URL`]
/// (never from the command line), the database name from `-d`. Shared with the live module
/// operations of `erp module`.
///
/// # Errors
///
/// [`CliError::InvalidInput`] when `DATABASE_URL` is not set, not UTF-8, or not a PostgreSQL
/// connection string. The message never contains the connection string.
pub(crate) fn connection_config(
    database: &crate::args::DatabaseName,
) -> Result<tokio_postgres::Config> {
    let invalid = |reason: &str| CliError::InvalidInput {
        arg: DATABASE_URL.to_owned(),
        reason: reason.to_owned(),
    };
    let url = std::env::var(DATABASE_URL).map_err(|e| match e {
        std::env::VarError::NotPresent => {
            invalid("not set: the connection credentials come from the environment")
        }
        std::env::VarError::NotUnicode(_) => invalid("not valid UTF-8"),
    })?;
    let mut config = tokio_postgres::Config::from_str(&url)
        .map_err(|_| invalid("not a PostgreSQL connection string (URL or key=value form)"))?;
    let database = erp_modules::system::DatabaseName::parse(database.as_str())
        .map_err(|_| CliError::InvalidDatabaseName(database.as_str().to_owned()))?;
    config.dbname(database.as_str());
    Ok(config)
}

/// The registry of the addons linked into this binary.
pub(crate) fn registry() -> Result<Arc<Registry>> {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([&erp_data::IR_MODEL_DATA])
        .build()?;
    check_registry(&registry)?;
    Ok(Arc::new(registry))
}

pub(crate) fn pool(config: tokio_postgres::Config) -> Result<deadpool_postgres::Pool> {
    pool_with_size(config, 1)
}

/// A connection pool of at most `size` connections.
pub(crate) fn pool_with_size(
    config: tokio_postgres::Config,
    size: usize,
) -> Result<deadpool_postgres::Pool> {
    erp_orm::pool::create_pool(config, size).map_err(|e| {
        CliError::Execution(format!("could not build the database connection pool: {e}"))
    })
}

async fn report(args: &AccessArgs) -> Result<String> {
    let config = config(args)?;
    let registry = registry()?;
    let pool = pool(config)?;
    let mut client = pool.get().await.map_err(|e| {
        CliError::Access(Box::new(erp_fields::Error::new(
            erp_fields::ErrorKind::Database,
            format!("could not connect to the database: {e}"),
        )))
    })?;
    let rows = load_rows_from(&mut client).await?;
    Ok(AccessReport::build(&rows, registry)?.to_string())
}

async fn disable(args: &AccessArgs, xmlid: &erp_orm::QualifiedXmlId) -> Result<String> {
    let config = config(args)?;
    let orm = Orm::builder(registry()?, pool(config)?).build()?;
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let rule = disable_rule(&mut tx, xmlid).await?;
    tx.commit().await?;
    Ok(rule.to_string())
}
