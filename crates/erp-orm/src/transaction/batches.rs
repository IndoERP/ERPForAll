//! The driver's counts of response batches, as the ORM hands them to tests (amendment
//! A3 and A4).

use deadpool::managed::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Scope {
    Connection(ObjectId),
    #[cfg(feature = "test-driver-process-batches")]
    Process,
}

/// A reading of the driver's counts on one connection since it connected
/// ([`crate::Transaction::driver_response_batches`]): the batches of response messages it
/// delivered, how many of those parked it on a full channel, and which pooled connection it was
/// taken on, or process-wide totals (`ResponseBatches::process_wide`, available only with the
/// `test-driver-process-batches` diagnostic feature). Both counts only grow
/// in one scope; the counts of a window are
/// [`ResponseBatches::since`] an earlier reading in the same scope. Read between requests:
/// the two counts are read one after the other, so a reading taken while a request is being
/// answered may see a batch in one count and not yet in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ResponseBatches {
    /// The connection or process scope; `None` for a window,
    /// which belongs to the scope of the readings it came from.
    scope: Option<Scope>,
    delivered: u64,
    parked: u64,
}

impl ResponseBatches {
    /// A reading on `connection` of `delivered` batches, `parked` of which parked it.
    pub(crate) const fn new(connection: ObjectId, delivered: u64, parked: u64) -> Self {
        Self {
            scope: Some(Scope::Connection(connection)),
            delivered,
            parked,
        }
    }

    /// Process-lifetime totals across every driver connection, including retired connections.
    /// Use this for an isolated allocation-test process when a service returns its connection
    /// before the caller can inspect it. No unrelated database work may run in the window.
    /// Read between completed requests: the two atomics are read separately, and parked batches
    /// are exact only on a current-thread runtime (validated by `erp_testkit::driver::DriverBlocks`).
    /// Diagnostic state only; never use it for admission or application correctness.
    ///
    /// ```
    /// use erp_orm::ResponseBatches;
    /// let before = ResponseBatches::process_wide();
    /// // Run one completed request in an otherwise quiescent test process.
    /// let window = ResponseBatches::process_wide().since(before).unwrap();
    /// assert!(window.parked() <= window.delivered());
    /// ```
    #[must_use]
    #[cfg(feature = "test-driver-process-batches")]
    pub fn process_wide() -> Self {
        let (delivered, parked) = tokio_postgres::Client::process_response_batches();
        Self {
            scope: Some(Scope::Process),
            delivered,
            parked,
        }
    }

    /// The counts of the window since `earlier`, or `None` when the two readings were taken on
    /// different connections or scopes, or either count of `earlier` is larger (a counter
    /// regression): their difference would mean nothing. A window (a result of `since`) has no
    /// scope of its own, so a window minus anything is `None` too.
    #[must_use]
    pub fn since(self, earlier: ResponseBatches) -> Option<ResponseBatches> {
        let (Some(now), Some(then)) = (self.scope, earlier.scope) else {
            return None;
        };
        if now != then {
            return None;
        }
        Some(Self {
            scope: None,
            delivered: self.delivered.checked_sub(earlier.delivered)?,
            parked: self.parked.checked_sub(earlier.parked)?,
        })
    }

    /// Batches delivered (`tokio_postgres::Client::response_batches`).
    #[must_use]
    pub const fn delivered(self) -> u64 {
        self.delivered
    }

    /// Of the delivered batches, those that parked the connection on a full channel
    /// (`tokio_postgres::Client::parked_batches`; exact on a current-thread runtime only).
    #[must_use]
    pub const fn parked(self) -> u64 {
        self.parked
    }
}

#[cfg(test)]
mod tests {
    use super::ResponseBatches;
    #[cfg(feature = "test-driver-process-batches")]
    use super::Scope;
    use deadpool::managed::ObjectId;

    /// Two distinct ids, as a pool hands them out (`ObjectId` has no public constructor).
    fn ids() -> (ObjectId, ObjectId) {
        struct Nothing;
        impl deadpool::managed::Manager for Nothing {
            type Type = ();
            type Error = std::convert::Infallible;
            async fn create(&self) -> Result<(), Self::Error> {
                Ok(())
            }
            async fn recycle(
                &self,
                _: &mut (),
                _: &deadpool::managed::Metrics,
            ) -> deadpool::managed::RecycleResult<Self::Error> {
                Ok(())
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let pool = deadpool::managed::Pool::<Nothing>::builder(Nothing)
                .max_size(2)
                .build()
                .unwrap();
            let (a, b) = (pool.get().await.unwrap(), pool.get().await.unwrap());
            (
                deadpool::managed::Object::id(&a),
                deadpool::managed::Object::id(&b),
            )
        })
    }

    #[test]
    #[cfg(feature = "test-driver-process-batches")]
    fn process_readings_have_their_own_scope_and_windows_cannot_be_readings() {
        // Synthetic readings test scope arithmetic without assuming the other libtest unit
        // tests in this binary have no driver work. Live counts are tested in the serialized
        // driver_response_batches integration binary instead.
        let start = ResponseBatches {
            scope: Some(Scope::Process),
            delivered: 7,
            parked: 3,
        };
        let end = ResponseBatches {
            scope: Some(Scope::Process),
            delivered: 12,
            parked: 5,
        };
        let window = end.since(start).unwrap();
        assert_eq!((window.delivered(), window.parked()), (5, 2));
        assert_eq!(start.since(end), None);
        let empty = start.since(start).unwrap();
        assert_eq!((empty.delivered(), empty.parked()), (0, 0));
        assert_eq!(window.since(start), None);
        assert_eq!(end.since(window), None);
        let (connection, _) = ids();
        let local = ResponseBatches::new(connection, 0, 0);
        assert_eq!(end.since(local), None);
        assert_eq!(local.since(start), None);
    }

    #[test]
    fn a_window_is_the_growth_on_one_connection() {
        let (one, other) = ids();
        let (a, b) = (
            ResponseBatches::new(one, 7, 3),
            ResponseBatches::new(one, 12, 5),
        );
        let window = b.since(a).unwrap();
        assert_eq!((window.delivered(), window.parked()), (5, 2));
        assert_eq!(
            a.since(a).map(|w| (w.delivered(), w.parked())),
            Some((0, 0))
        );
        // a count that shrank: not one connection's history
        assert_eq!(a.since(b), None);
        assert_eq!(ResponseBatches::new(one, 12, 1).since(a), None);
        assert_eq!(ResponseBatches::new(one, 6, 5).since(a), None);
        // another connection, even with larger counts
        assert_eq!(ResponseBatches::new(other, 100, 50).since(a), None);
        // a window has no connection: nothing is measured against it
        assert_eq!(window.since(a), None);
        assert_eq!(b.since(window), None);
        let all = ResponseBatches::new(one, u64::MAX, u64::MAX)
            .since(ResponseBatches::new(one, 0, 0))
            .unwrap();
        assert_eq!((all.delivered(), all.parked()), (u64::MAX, u64::MAX));
    }
}
