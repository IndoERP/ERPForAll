//! Concurrency tests: two transactions on
//! their own connections, and a wait until one of them is queued behind the other's lock.
//!
//! ```ignore
//! let (mut a, mut b) = h.begin_pair().await?;
//! upsert_on(&mut a).await?;                                // A holds the new row's lock
//! let pid = erp_testkit::backend_pid(&mut b).await?;       // B's snapshot starts here
//! let waiting = tokio::spawn(async move { (upsert_on(&mut b).await, b) });
//! erp_testkit::wait_until_blocked(&h, pid, Duration::from_secs(10)).await?;
//! a.commit().await?;                                       // B wakes up: 40001 under REPEATABLE READ
//! let (result, b) = waiting.await?;
//! ```

use std::time::{Duration, Instant};

use erp_orm::Transaction;

use crate::error::TestkitError;
use crate::harness::{Harness, HarnessError};

/// How often [`wait_until_blocked`] looks at `pg_stat_activity`.
const POLL: Duration = Duration::from_millis(5);

impl Harness {
    /// Two transactions as the harness principal ([`Harness::principal`]), each on its own
    /// pooled connection (the testkit's pools hold four). Each takes its `REPEATABLE READ`
    /// snapshot at its first statement.
    pub async fn begin_pair(&self) -> Result<(Transaction, Transaction), HarnessError> {
        Ok((self.begin().await?, self.begin().await?))
    }
}

/// The server process id of `tx`'s connection. Sends one statement, which opens the transaction
/// and so takes its snapshot.
pub async fn backend_pid(tx: &mut Transaction) -> Result<i32, HarnessError> {
    let rows = tx.query("SELECT pg_backend_pid()", &[]).await?;
    let Some(row) = rows.first() else {
        return Err(HarnessError::Timeout(
            "pg_backend_pid() returned no row".to_owned(),
        ));
    };
    row.try_get::<_, i32>(0).map_err(|source| {
        HarnessError::Testkit(TestkitError::Postgres {
            action: "reading pg_backend_pid()".to_owned(),
            source,
        })
    })
}

/// Wait until the server process `pid` waits for a lock held by another transaction
/// (`pg_stat_activity.wait_event_type = 'Lock'`), looking every 5 ms through a connection of
/// `harness`'s pool (each look is its own statement, so it sees the current activity).
///
/// # Errors
///
/// [`HarnessError::Timeout`] when the process does not wait for a lock within `timeout`;
/// [`HarnessError::Testkit`] when a look fails.
pub async fn wait_until_blocked(
    harness: &Harness,
    pid: i32,
    timeout: Duration,
) -> Result<(), HarnessError> {
    let client = harness.db().client().await?;
    let started = Instant::now();
    loop {
        let row = client
            .query_opt(
                "SELECT wait_event_type FROM pg_stat_activity WHERE pid = $1",
                &[&pid],
            )
            .await
            .map_err(|source| {
                HarnessError::Testkit(TestkitError::Postgres {
                    action: format!("reading pg_stat_activity for process {pid}"),
                    source,
                })
            })?;
        let waiting: Option<String> = match &row {
            Some(row) => row.try_get(0).map_err(|source| {
                HarnessError::Testkit(TestkitError::Postgres {
                    action: format!("decoding the wait event of process {pid}"),
                    source,
                })
            })?,
            None => {
                return Err(HarnessError::Timeout(format!(
                    "process {pid} is not connected"
                )));
            }
        };
        if waiting.as_deref() == Some("Lock") {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(HarnessError::Timeout(format!(
                "process {pid} did not wait for a lock within {timeout:?} (wait event: {waiting:?})"
            )));
        }
        tokio::time::sleep(POLL).await;
    }
}
