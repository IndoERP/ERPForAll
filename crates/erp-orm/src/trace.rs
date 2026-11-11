//! The request trace (SPEC-ERRTRACE): the id of the request being served and the WebAssembly
//! addon it last entered, readable from any code that runs inside the request's task, so a log
//! line written deep in the stack (a guest import, a trap) can say which click it belongs to.
//!
//! Adopted: the `unit_of_work` task-local pattern of
//! this crate (a scope per call, nothing authoritative in it). Modified: the id is carried
//! with the call and also returned to the client, and the scope records the addon so the RPC
//! layer can name it in its own refusal line.
//!
//! Like [`crate::unit_of_work`], the scope does not cross `tokio::spawn`.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

/// One request's trace: written by the transport (`id`), by the WebAssembly host (`addon`), and
/// read by whoever logs.
#[derive(Debug, Default)]
pub struct RequestTrace {
    id: Option<Arc<str>>,
    addon: Mutex<Option<Arc<str>>>,
}

impl RequestTrace {
    /// A trace for a request with this id (`None`: the transport gave none).
    pub fn new(id: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            id: id.map(Arc::from),
            addon: Mutex::new(None),
        })
    }

    /// The request id, when there is one.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The WebAssembly addon the request entered last, when it entered one.
    pub fn addon(&self) -> Option<Arc<str>> {
        self.addon
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_addon(&self, name: &str) {
        let mut slot = self.addon.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_deref() != Some(name) {
            *slot = Some(Arc::from(name));
        }
    }
}

tokio::task_local! {
    static TRACE: Arc<RequestTrace>;
}

/// Run `work` with `trace` as the current request trace.
pub async fn with_request_trace<F: Future>(trace: Arc<RequestTrace>, work: F) -> F::Output {
    TRACE.scope(trace, work).await
}

/// The current request trace, if the code runs inside [`with_request_trace`].
pub fn current_request_trace() -> Option<Arc<RequestTrace>> {
    TRACE.try_with(Arc::clone).ok()
}

/// The current request's id, if any.
pub fn current_request_id() -> Option<Arc<str>> {
    TRACE.try_with(|t| t.id.clone()).ok().flatten()
}

/// Record that the current request entered the WebAssembly addon `name` (a no-op outside a
/// request trace).
pub fn note_wasm_addon(name: &str) {
    // Specified: outside a request (a unit test, a background job) there is nothing to record
    // into, and that is not an error.
    TRACE.try_with(|t| t.set_addon(name)).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_trace_is_visible_inside_the_scope_only() {
        assert!(current_request_id().is_none());
        note_wasm_addon("ignored");
        let trace = RequestTrace::new(Some("0194f5a0-7b1c-7d2e-8f3a-123456789abc"));
        let seen = Arc::clone(&trace);
        with_request_trace(trace, async {
            assert_eq!(
                current_request_id().as_deref(),
                Some("0194f5a0-7b1c-7d2e-8f3a-123456789abc")
            );
            note_wasm_addon("lending");
        })
        .await;
        assert_eq!(seen.addon().as_deref(), Some("lending"));
        assert!(current_request_id().is_none());
    }
}
