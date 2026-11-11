//! Explicit database fixtures for tests that install the `base` addon's schema.
//!
//! These helpers do not load production defaults or install addon data. Each call inserts a
//! fresh test row and fails on a conflict, so test setup cannot silently reuse another row.

use erp_fields::{Error, Result};
use erp_orm::Transaction;
use tokio_postgres::Client;

const CURRENCY_SQL: &str = "INSERT INTO res_currency (name, symbol, rounding, active) \
    VALUES ('USD', '$', 0.01, true) RETURNING id::int8";

/// Insert the USD fixture with its sequence-generated id using a PostgreSQL client.
///
/// The caller must have installed the base currency schema and must use the returned id for
/// company foreign keys. Call once per fixture; an existing USD is an error.
///
/// # Errors
/// Returns the database error on insertion failure or the driver error on id decoding failure.
///
/// ```no_run
/// # async fn example(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
/// let currency_id = erp_testkit::fixtures::seed_currency(client).await?;
/// client.execute(
///     "INSERT INTO res_company (name, currency_id) VALUES ('Test', $1)",
///     &[&currency_id],
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub async fn seed_currency(client: &Client) -> std::result::Result<i64, tokio_postgres::Error> {
    client.query_one(CURRENCY_SQL, &[]).await?.try_get(0)
}

/// Insert the same fixture as [`seed_currency`] within an ORM transaction.
///
/// # Errors
/// Propagates the transaction's database error. Returns `Programming` if the insertion does
/// not return exactly one row, or `Database` with the driver source if the id cannot be decoded.
///
/// ```no_run
/// # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
/// let currency_id = erp_testkit::fixtures::seed_currency_in(tx).await?;
/// tx.execute(
///     "INSERT INTO res_company (name, currency_id) VALUES ('Test', $1)",
///     &[&currency_id],
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub async fn seed_currency_in(tx: &mut Transaction) -> Result<i64> {
    let rows = tx.query(CURRENCY_SQL, &[]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(
            "currency fixture insertion must return exactly one id",
        ));
    };
    row.try_get(0)
        .map_err(|source| Error::db("cannot decode currency fixture id").with_source(source))
}
