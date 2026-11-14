// SPDX-License-Identifier: MIT OR Apache-2.0
//! `erp db drop | dump | restore | neutralize` (design,
//! spec DBLIFE): one named database, on the owner
//! connection of a deployment configuration (`--config`; only its host, port, role and password
//! are used, the database comes from `-d`).
//!
//! - Every command refuses a database that has a running node ([`refuse_live`]) unless
//!   `--force-live` is given.
//! - Nothing printed names a connection URL, a password or a role secret: errors carry the
//!   PostgreSQL message only ([`pg_error`]), and `pg_dump`/`pg_restore` get the credential through
//!   their environment, never their argument list.
//! - `neutralize` is our own list ([`HOUSEKEEPING_JOB_KINDS`] and the statements of
//!   [`neutralize`]); it runs in one transaction and can be repeated.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use deadpool_postgres::{Object, Pool};
use tokio_postgres::config::Host;

use crate::access::pool_with_size;
use crate::args::{DbTargetArgs, DumpArgs, NeutralizeArgs, RestoreArgs};
use crate::error::{CliError, Result};
use crate::human::{config, failed, runtime};

/// Job kinds a neutralized database keeps running: they only keep the job queue's own tables
/// short (dead letters past their retention, run history past its retention) and reach nothing
/// outside the database. Every other kind is held: scheduled actions (`base.cron`), scheduled
/// messages, and the attachment store jobs, whose store may be shared with the database this
/// copy came from (a sweep on the copy could remove content the original still uses).
pub const HOUSEKEEPING_JOB_KINDS: [&str; 2] =
    ["core.jobs_dead_expiry", "core.jobs_history_retention"];

/// The tables `neutralize` changes; a database without one of them is refused before anything
/// changes (it was not initialized by this version).
const NEUTRALIZE_TABLES: [&str; 7] = [
    "ir_cron",
    "erp_job",
    "erp_session",
    "res_users_apikeys",
    "erp_login_throttle",
    "res_users",
    "ir_config_parameter",
];

/// The system parameter a neutralized database carries: a marker for operators and tools.
/// Nothing reads it yet (spec DBLIFE, section 3).
pub const NEUTRALIZED_PARAMETER: &str = "database.neutralized";

/// What `restore` appends to the target's name for the database it restores into before that
/// database takes the target's name.
const RESTORE_SUFFIX: &str = "_erp_restore";

/// What `restore --replace` appends to the target's name for the database it replaces, until the
/// restored one has the target's name.
const REPLACED_SUFFIX: &str = "_erp_replaced";

/// PostgreSQL's longest identifier in bytes (`NAMEDATALEN - 1`); a longer one is cut silently.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// What a failed PostgreSQL client program's error ends with.
const PG_BIN_HINT: &str = "if the client programs are older than the server, name the directory of the PostgreSQL 18 client programs with --pg-bin or ERP_PG_BIN";

/// Databases no command of this module acts on: the maintenance database it connects to and the
/// templates every `CREATE DATABASE` copies.
const SYSTEM_DATABASES: [&str; 3] = ["postgres", "template0", "template1"];

/// The PostgreSQL message of a database error, without connection details; a connection that
/// failed before the server answered says only that.
fn pg_error(context: &str, error: &tokio_postgres::Error) -> CliError {
    match error.as_db_error() {
        Some(db) => failed(format!(
            "{context}: {} (SQLSTATE {})",
            db.message(),
            db.code().code()
        )),
        None => {
            // the kind of an I/O cause tells refused, timed out and name lookup apart; its
            // message is not printed
            let cause = std::error::Error::source(error)
                .and_then(|source| source.downcast_ref::<std::io::Error>())
                .map(|io| format!(" ({})", io.kind()))
                .unwrap_or_default();
            failed(format!(
                "{context}: the database connection failed: {error}{cause}"
            ))
        }
    }
}

fn invalid(arg: &str, reason: String) -> CliError {
    CliError::InvalidInput {
        arg: arg.to_owned(),
        reason,
    }
}

/// The owner connection of `config` pointed at `database`.
fn connection(config_path: &Path, database: &str) -> Result<tokio_postgres::Config> {
    let config = config(config_path)?;
    let mut connection = config.owner_connection();
    connection.dbname(database);
    Ok(connection)
}

async fn connect(pool: &Pool, database: &str) -> Result<Object> {
    pool.get().await.map_err(|error| match error {
        deadpool_postgres::PoolError::Backend(error) => pg_error(
            &format!("the connection to database {database} failed"),
            &error,
        ),
        other => failed(format!(
            "the connection to database {database} failed: {other}"
        )),
    })
}

/// A connection to the maintenance database `postgres` (for `CREATE`/`DROP DATABASE`).
async fn maintenance(config_path: &Path) -> Result<Object> {
    let pool = pool_with_size(connection(config_path, "postgres")?, 1)?;
    connect(&pool, "postgres").await
}

async fn exists(client: &Object, database: &str) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT FROM pg_database WHERE datname = $1)",
            &[&database],
        )
        .await
        .map_err(|e| pg_error("could not list the databases", &e))?;
    Ok(row.get(0))
}

fn checked_name(target: &DbTargetArgs) -> Result<&str> {
    let name = target.database.as_str();
    if SYSTEM_DATABASES.contains(&name) {
        return Err(invalid(
            "-d",
            format!(
                "{name} is a system database of the PostgreSQL cluster; no erp db command acts on it"
            ),
        ));
    }
    Ok(name)
}

/// Refuse `database` when a node registered in it is running: its row in `erp_node` is not
/// `stopped` and its lease has not expired. A database without the node table never served.
async fn refuse_live(client: &Object, database: &str, force_live: bool) -> Result<()> {
    let registry: bool = client
        .query_one("SELECT to_regclass('public.erp_node') IS NOT NULL", &[])
        .await
        .map_err(|e| pg_error("could not inspect the node registry", &e))?
        .get(0);
    let nodes: Vec<String> = if registry {
        client
            .query(
                "SELECT name FROM erp_node WHERE state <> 'stopped' AND expires_at > now() \
                 ORDER BY name",
                &[],
            )
            .await
            .map_err(|e| pg_error("could not read the node registry", &e))?
            .iter()
            .map(|row| row.get(0))
            .collect()
    } else {
        Vec::new()
    };
    if nodes.is_empty() || force_live {
        return Ok(());
    }
    Err(invalid(
        "-d",
        format!(
            "database {database} has a running node ({}); stop it first, or pass --force-live if you mean to act on a live database",
            nodes.join(", ")
        ),
    ))
}

/// `erp db drop`.
pub(crate) fn drop_db(args: &DbTargetArgs) -> Result<String> {
    runtime()?.block_on(drop_async(args))
}

async fn drop_async(args: &DbTargetArgs) -> Result<String> {
    let name = checked_name(args)?;
    let admin = maintenance(&args.config).await?;
    if !exists(&admin, name).await? {
        return Err(invalid("-d", format!("database {name} does not exist")));
    }
    check_target(&args.config, name, args.force_live).await?;
    drop_database(&admin, name, args.force_live).await?;
    Ok(format!("Database {name} dropped."))
}

/// The live check on the target database itself.
async fn check_target(config_path: &Path, name: &str, force_live: bool) -> Result<()> {
    let pool = pool_with_size(connection(config_path, name)?, 1)?;
    let client = connect(&pool, name).await?;
    refuse_live(&client, name, force_live).await?;
    drop(client);
    pool.close();
    Ok(())
}

async fn drop_database(admin: &Object, name: &str, force: bool) -> Result<()> {
    // `WITH (FORCE)` ends other sessions; only an operator who said `--force-live` gets it.
    let statement = format!(
        "DROP DATABASE {}{}",
        erp_sql::quote(name),
        if force { " WITH (FORCE)" } else { "" }
    );
    admin
        .batch_execute(&statement)
        .await
        .map_err(|e| pg_error(&format!("could not drop database {name}"), &e))
}

/// The PostgreSQL client program `name`: in `--pg-bin` (or `ERP_PG_BIN`) when given, otherwise
/// the first one on `PATH`.
fn pg_tool(pg_bin: Option<&Path>, name: &str) -> Result<PathBuf> {
    let found = match pg_bin {
        Some(dir) => Some(dir.join(name)).filter(|path| path.is_file()),
        None => std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
        }),
    };
    found.ok_or_else(|| {
        invalid(
            "--pg-bin",
            format!("{name} not found; name the directory of the PostgreSQL client programs with --pg-bin or ERP_PG_BIN"),
        )
    })
}

/// Run a PostgreSQL client program with the credential of `connection` in its environment.
/// Its standard error is returned in the error; the programs print no password (they never
/// receive a URL).
fn run_tool(
    program: &Path,
    args: &[&std::ffi::OsStr],
    connection: &tokio_postgres::Config,
) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    // no libpq setting of the operator's shell reaches the tool: only the connection below
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"PG") {
            command.env_remove(key);
        }
    }
    match connection.get_hosts().first() {
        Some(Host::Tcp(host)) => command.env("PGHOST", host),
        Some(Host::Unix(path)) => command.env("PGHOST", path),
        None => return Err(failed("the owner connection names no host")),
    };
    if let Some(port) = connection.get_ports().first() {
        command.env("PGPORT", port.to_string());
    }
    let user = connection
        .get_user()
        .ok_or_else(|| failed("the owner connection names no role"))?;
    command.env("PGUSER", user);
    if let Some(password) = connection.get_password() {
        let password = std::str::from_utf8(password)
            .map_err(|_| failed("the owner connection's password is not UTF-8"))?;
        command.env("PGPASSWORD", password);
    }
    let output = command.output().map_err(|e| CliError::Io {
        path: program.to_path_buf(),
        source: e,
    })?;
    if output.status.success() {
        return Ok(());
    }
    Err(failed(format!(
        "{} failed ({}): {}; {PG_BIN_HINT}",
        program.file_name().map_or_else(
            || program.display().to_string(),
            |n| n.to_string_lossy().into_owned()
        ),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// `erp db dump`.
pub(crate) fn dump(args: &DumpArgs) -> Result<String> {
    runtime()?.block_on(dump_async(args))
}

async fn dump_async(args: &DumpArgs) -> Result<String> {
    let name = checked_name(&args.target)?;
    let program = pg_tool(args.pg_bin.as_deref(), "pg_dump")?;
    let admin = maintenance(&args.target.config).await?;
    if !exists(&admin, name).await? {
        return Err(invalid("-d", format!("database {name} does not exist")));
    }
    check_target(&args.target.config, name, args.target.force_live).await?;
    // a configuration that cannot name the connection fails before the file exists
    let connection = connection(&args.target.config, name)?;
    // The archive holds every row: it is created for the owner only, and never over a file.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.file)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => invalid(
                "--file",
                format!(
                    "{} exists; a dump never replaces a file",
                    args.file.display()
                ),
            ),
            _ => CliError::Io {
                path: args.file.clone(),
                source: e,
            },
        })?;
    let result = run_tool(
        &program,
        &[
            "--format=custom".as_ref(),
            "--file".as_ref(),
            args.file.as_os_str(),
            "--dbname".as_ref(),
            name.as_ref(),
        ],
        &connection,
    );
    if let Err(error) = result {
        // a partial archive is not a dump
        return Err(match std::fs::remove_file(&args.file) {
            Ok(()) => error,
            Err(remove) => failed(format!(
                "{error}; the partial file {} could not be removed: {remove}",
                args.file.display()
            )),
        });
    }
    Ok(format!(
        "Database {name} dumped to {}.",
        args.file.display()
    ))
}

/// `erp db restore`.
pub(crate) fn restore(args: &RestoreArgs) -> Result<String> {
    runtime()?.block_on(restore_async(args))
}

/// The database `restore` writes into before it takes `name`, and the name the replaced database
/// has until then: bounded so PostgreSQL never cuts either to another name.
fn restore_names(name: &str) -> Result<(String, String)> {
    let staging = format!("{name}{RESTORE_SUFFIX}");
    let replaced = format!("{name}{REPLACED_SUFFIX}");
    if staging.len().max(replaced.len()) > MAX_IDENTIFIER_BYTES {
        return Err(invalid(
            "-d",
            format!(
                "{name} is too long to restore: the restore uses the names {staging} and {replaced}, and PostgreSQL names have at most {MAX_IDENTIFIER_BYTES} bytes (use a name of at most {} bytes)",
                MAX_IDENTIFIER_BYTES - RESTORE_SUFFIX.len().max(REPLACED_SUFFIX.len())
            ),
        ));
    }
    Ok((staging, replaced))
}

/// End the other sessions on `database` (`--force-live`; a rename refuses while any is connected).
async fn end_sessions(admin: &Object, database: &str) -> Result<()> {
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&database],
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            pg_error(
                &format!("could not end the sessions on database {database}"),
                &e,
            )
        })
}

/// `ALTER DATABASE from RENAME TO to`.
async fn rename_database(admin: &Object, from: &str, to: &str) -> Result<()> {
    admin
        .batch_execute(&format!(
            "ALTER DATABASE {} RENAME TO {}",
            erp_sql::quote(from),
            erp_sql::quote(to)
        ))
        .await
        .map_err(|e| pg_error(&format!("could not rename database {from} to {to}"), &e))
}

/// Drop the temporary database of a failed restore; `target` says what happened to the target
/// database (empty when the restore was to a new name).
async fn abandon(admin: &Object, staging: &str, target: &str, error: CliError) -> CliError {
    match drop_database(admin, staging, false).await {
        Ok(()) => failed(format!(
            "{error}; the temporary database {staging} was dropped{target}"
        )),
        Err(drop) => failed(format!(
            "{error}; the temporary database {staging} remains ({drop}){target}"
        )),
    }
}

async fn restore_async(args: &RestoreArgs) -> Result<String> {
    let name = checked_name(&args.target)?;
    let (staging, replaced) = restore_names(name)?;
    let program = pg_tool(args.pg_bin.as_deref(), "pg_restore")?;
    if !args.file.is_file() {
        return Err(invalid(
            "--file",
            format!("{} is not a file", args.file.display()),
        ));
    }
    let connection = connection(&args.target.config, &staging)?;
    // reading the archive's table of contents refuses a file that is not an archive, or one this
    // pg_restore cannot read, before anything changes
    run_tool(
        &program,
        &["--list".as_ref(), "--".as_ref(), args.file.as_os_str()],
        &connection,
    )
    .map_err(|error| {
        invalid(
            "--file",
            format!(
                "{} is not an archive this pg_restore can read; nothing was changed: {error}",
                args.file.display()
            ),
        )
    })?;
    let admin = maintenance(&args.target.config).await?;
    let replace = exists(&admin, name).await?;
    if replace {
        if !args.replace {
            return Err(invalid(
                "-d",
                format!("database {name} exists; pass --replace to drop it and restore over it"),
            ));
        }
        check_target(&args.target.config, name, args.target.force_live).await?;
    }
    // the moved-aside original is checked first: after a restore stopped between its two renames
    // it is the only copy of what the target was
    let staging_left = exists(&admin, &staging).await?;
    if exists(&admin, &replaced).await? {
        let archive = if staging_left {
            format!(" {staging} holds the archive that restore was writing.")
        } else {
            String::new()
        };
        let message = if replace {
            format!(
                "database {replaced} exists: it holds what {name} was before an earlier restore that stopped before it finished, or before one running now.{archive} If none is running and {name} is the database you want, drop {replaced} with erp db drop -d {replaced}; otherwise rename it back to {name} (ALTER DATABASE); then restore again"
            )
        } else {
            format!(
                "database {name} does not exist and {replaced} holds what it was: an earlier restore stopped between its two renames, or one is running now.{archive} If none is running, rename {replaced} back to {name} (ALTER DATABASE {replaced} RENAME TO {name}) to keep the original, then restore again"
            )
        };
        return Err(invalid("-d", message));
    }
    if staging_left {
        return Err(invalid(
            "-d",
            format!(
                "database {staging} exists: an earlier restore of {name} stopped before it finished, or one is running now. If none is running, drop it with erp db drop -d {staging}, then restore again"
            ),
        ));
    }
    // The archive goes into a new database first: the existing one is replaced only once the
    // archive restored.
    admin
        .batch_execute(&format!(
            "CREATE DATABASE {} {}",
            erp_sql::quote(&staging),
            erp_modules::DATABASE_CREATE_OPTIONS
        ))
        .await
        .map_err(|e| pg_error(&format!("could not create database {staging}"), &e))?;
    // Owners and grants of the source are not restored: the restoring role owns everything.
    let result = run_tool(
        &program,
        &[
            "--exit-on-error".as_ref(),
            "--no-owner".as_ref(),
            "--no-privileges".as_ref(),
            "--dbname".as_ref(),
            staging.as_ref(),
            // a file name that starts with `-` is not read as an option
            "--".as_ref(),
            args.file.as_os_str(),
        ],
        &connection,
    );
    let untouched = if replace {
        format!("; the existing database {name} was left untouched")
    } else {
        String::new()
    };
    if let Err(error) = result {
        return Err(abandon(&admin, &staging, &untouched, error).await);
    }
    let force = args.target.force_live;
    if !replace {
        rename_database(&admin, &staging, name)
            .await
            .map_err(|error| {
                failed(format!(
                    "{error}; the restored archive is in database {staging}: rename it to {name}"
                ))
            })?;
    } else {
        // a node may have started while the archive restored; the existing database moves aside
        // and is dropped only once the restored one has its name
        if let Err(error) = check_target(&args.target.config, name, force).await {
            return Err(abandon(&admin, &staging, &untouched, error).await);
        }
        let ended = if force {
            if let Err(error) = end_sessions(&admin, name).await {
                let target = format!(
                    "; the existing database {name} keeps its name and data, but some of its other sessions may have been ended (--force-live)"
                );
                return Err(abandon(&admin, &staging, &target, error).await);
            }
            ", and --force-live ended its other sessions"
        } else {
            ""
        };
        if let Err(error) = rename_database(&admin, name, &replaced).await {
            let target = format!("; the existing database {name} keeps its name and data{ended}");
            return Err(abandon(&admin, &staging, &target, error).await);
        }
        if let Err(error) = rename_database(&admin, &staging, name).await {
            return Err(match rename_database(&admin, &replaced, name).await {
                Ok(()) => {
                    let target = format!(
                        "; the existing database was renamed to {replaced} and back to {name} with its data unchanged{ended}"
                    );
                    abandon(&admin, &staging, &target, error).await
                }
                Err(back) => failed(format!(
                    "{error}; renaming {replaced} back to {name} failed too ({back}): the existing database is in {replaced} and the restored archive in {staging}; rename one of them to {name}"
                )),
            });
        }
        drop_database(&admin, &replaced, force)
            .await
            .map_err(|error| {
                failed(format!(
                    "database {name} was restored, but the database it replaced remains as {replaced} ({error}); drop it with erp db drop -d {replaced}"
                ))
            })?;
    }
    Ok(format!(
        "Database {name} restored from {}; run erp db publish-grants for its runtime role (it refuses while an installed addon declares a SECURITY DEFINER function: the restore made the owner role its owner).",
        args.file.display()
    ))
}

/// Refuse the database a copy was made from. A database records its physical identity (cluster
/// system identifier and database OID) in `ir_blob_owner_identity` the first time an attachment
/// ownership step runs in it (`erp_addon_base` ownership `identity`); a restored or templated
/// copy has another OID, so a record equal to the current identity means this is that database
/// itself, or a physical copy of its whole cluster (`--physical-copy`). Without a record (no
/// table or no row) a copy cannot be told from its original: refused unless
/// `--unrecorded-identity` is given, and then the returned line says the check was skipped.
async fn refuse_original(
    client: &Object,
    name: &str,
    physical_copy: bool,
    unrecorded_identity: bool,
) -> Result<Option<String>> {
    let table: bool = client
        .query_one(
            "SELECT to_regclass('public.ir_blob_owner_identity') IS NOT NULL",
            &[],
        )
        .await
        .map_err(|e| pg_error("could not inspect the database", &e))?
        .get(0);
    let same: Option<bool> = if table {
        recorded_identity_matches(client).await?
    } else {
        None
    };
    let Some(same) = same else {
        if unrecorded_identity {
            return Ok(Some(format!(
                "Database {name} has no recorded identity; the check that it is a copy was skipped (--unrecorded-identity)."
            )));
        }
        return Err(invalid(
            "-d",
            format!(
                "database {name} has no recorded identity, so neutralize cannot tell a copy from the original it came from; nothing was changed. Pass --unrecorded-identity only when you know {name} is a copy"
            ),
        ));
    };
    if same && !physical_copy {
        return Err(invalid(
            "-d",
            format!(
                "database {name} is the database its recorded identity names (same cluster and database OID), not a copy; nothing was changed. Pass --physical-copy only when it is a physical copy of its whole cluster (a base backup or a volume snapshot)"
            ),
        ));
    }
    Ok(None)
}

/// Whether the recorded identity row names this database; `None` when there is no row.
async fn recorded_identity_matches(client: &Object) -> Result<Option<bool>> {
    Ok(client
        .query_opt(
            "SELECT i.system_identifier::text = p.system_identifier::text \
                    AND i.database_oid = d.oid::int8 \
             FROM ir_blob_owner_identity i CROSS JOIN pg_control_system() p \
             JOIN pg_database d ON d.datname = current_database() \
             WHERE i.singleton",
            &[],
        )
        .await
        .map_err(|e| pg_error("could not read the recorded database identity", &e))?
        .map(|row| row.get(0)))
}

/// `erp db neutralize`.
pub(crate) fn neutralize(args: &NeutralizeArgs) -> Result<String> {
    runtime()?.block_on(neutralize_async(args))
}

async fn neutralize_async(args: &NeutralizeArgs) -> Result<String> {
    let name = checked_name(&args.target)?;
    let password = erp_server::config::read_secret(&args.password_file).map_err(|_| {
        invalid(
            "--password-file",
            "must be a mode-600 regular file holding the throwaway password".to_owned(),
        )
    })?;
    if password.len() < 12 || password.chars().any(char::is_control) {
        return Err(invalid(
            "--password-file",
            "the throwaway password needs at least 12 bytes and no control characters".to_owned(),
        ));
    }
    let pool = pool_with_size(connection(&args.target.config, name)?, 1)?;
    let mut client = connect(&pool, name).await?;
    refuse_live(&client, name, args.target.force_live).await?;
    for table in NEUTRALIZE_TABLES {
        let present: bool = client
            .query_one(
                "SELECT to_regclass($1) IS NOT NULL",
                &[&format!("public.{table}")],
            )
            .await
            .map_err(|e| pg_error("could not inspect the database", &e))?
            .get(0);
        if !present {
            return Err(invalid(
                "-d",
                format!(
                    "database {name} has no table {table}: it is not a database initialized by this version; nothing was changed"
                ),
            ));
        }
    }
    let skipped =
        refuse_original(&client, name, args.physical_copy, args.unrecorded_identity).await?;
    let hash = erp_security::auth::password::hash(
        password.as_bytes(),
        &erp_security::auth::password::HashParams::DEFAULT,
    )
    .map_err(|e| failed(format!("could not hash the throwaway password: {e}")))?;
    let secret = erp_security::auth::secret::DatabaseSecret::generate_text()
        .map_err(|e| failed(format!("could not generate a database secret: {e}")))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| pg_error("could not begin the neutralization", &e))?;
    let step = |what: &'static str| move |e: tokio_postgres::Error| pg_error(what, &e);
    // Scheduled actions off; their job rows are held below like every other kind.
    let crons = tx
        .execute("UPDATE ir_cron SET active = false WHERE active", &[])
        .await
        .map_err(step("could not switch off the scheduled actions"))?;
    // Every job but the queue's own housekeeping is held; a running row of the copy belongs to a
    // node that does not exist here, so its lease is cleared with it.
    let jobs = tx
        .execute(
            "UPDATE erp_job SET state = 'held', lease_owner = NULL, lease_until = NULL \
             WHERE state IN ('ready', 'running') AND kind <> ALL($1::text[])",
            &[&HOUSEKEEPING_JOB_KINDS.as_slice()],
        )
        .await
        .map_err(step("could not hold the jobs"))?;
    let sessions = tx
        .execute("DELETE FROM erp_session", &[])
        .await
        .map_err(step("could not remove the sessions"))?;
    let keys = tx
        .execute("DELETE FROM res_users_apikeys", &[])
        .await
        .map_err(step("could not remove the API keys"))?;
    // The counters are keyed with the secret that is replaced below: they mean nothing after it.
    let throttle = tx
        .execute("DELETE FROM erp_login_throttle", &[])
        .await
        .map_err(step("could not clear the login throttle"))?;
    let passwords = tx
        .execute(
            "UPDATE res_users SET password = $1 WHERE password IS NOT NULL",
            &[&hash],
        )
        .await
        .map_err(step("could not replace the passwords"))?;
    // A copy never shares the signing secret of the database it came from.
    let rotated = tx
        .execute(
            "UPDATE ir_config_parameter SET value = $1 WHERE key = 'database.secret'",
            &[&secret.as_str()],
        )
        .await
        .map_err(step("could not replace the database secret"))?;
    if rotated != 1 {
        return Err(failed(format!(
            "database {name} has no database.secret parameter; nothing was changed"
        )));
    }
    tx.execute(
        "INSERT INTO ir_config_parameter (key, value) VALUES ($1, 'true') \
         ON CONFLICT (key) DO UPDATE SET value = 'true'",
        &[&NEUTRALIZED_PARAMETER],
    )
    .await
    .map_err(step("could not set the neutralized flag"))?;
    tx.commit()
        .await
        .map_err(|e| pg_error("could not commit the neutralization", &e))?;
    let skipped = skipped.map(|line| format!("{line}\n")).unwrap_or_default();
    Ok(format!(
        "{skipped}Database {name} neutralized: {} scheduled actions switched off, {} jobs held (kept: {}), \
         {} sessions and {} API keys removed, {} login throttle rows cleared, {} passwords replaced \
         by the throwaway password, database secret replaced, {NEUTRALIZED_PARAMETER} = true.",
        crons,
        jobs,
        HOUSEKEEPING_JOB_KINDS.join(", "),
        sessions,
        keys,
        throttle,
        passwords,
    ))
}
