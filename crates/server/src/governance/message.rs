//! One message on a `/sync` websocket, governed like the HTTP request it stands in for.
//!
//! The upgrade is a single HTTP request, and after it every mutation and every live query is a
//! frame the HTTP layers never see. Without this, a caller refused at `/Patient` could write
//! through `/sync` as fast as the socket would carry it, unlimited, unmetered and unaudited.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use super::scope::{within, RequestScope};
use super::{audit, Class, Finished, Rejection};

pub struct SyncMessage {
    scope: Option<Arc<RequestScope>>,
    started: Instant,
    class: Class,
    path: String,
    bytes_in: u64,
}

impl SyncMessage {
    /// A message on a connection whose opening request had `connection` for its scope. With
    /// none (governance off) every method does nothing.
    pub fn begin(
        connection: Option<&Arc<RequestScope>>,
        class: Class,
        path: &str,
        bytes_in: usize,
    ) -> Self {
        Self {
            scope: connection.map(|scope| scope.message()),
            started: Instant::now(),
            class,
            path: path.to_string(),
            bytes_in: bytes_in as u64,
        }
    }

    /// The caller's quota and rate, as for an HTTP request of the same class. A refused message
    /// is counted and logged here; the caller only has to tell the client.
    pub async fn admit(&self) -> Result<(), Rejection> {
        let Some(scope) = &self.scope else {
            return Ok(());
        };
        match scope.governance().admit(scope, self.class).await {
            Ok(()) => Ok(()),
            Err(rejection) => {
                self.finish(scope, 429, 0);
                Err(rejection)
            }
        }
    }

    /// Run the work that serves the message as part of its request, so that the engine can
    /// attribute what it does.
    pub async fn run<F: Future>(&self, work: F) -> F::Output {
        match &self.scope {
            Some(scope) => within(scope.clone(), work).await,
            None => work.await,
        }
    }

    /// A replica sent the mutation on to the primary, which counts and audits it as a message
    /// of its own; it is only logged here.
    pub fn forwarded(&self) {
        if let Some(scope) = &self.scope {
            scope.mark_forwarded();
        }
    }

    /// The message was refused because the caller's scopes do not allow it.
    pub fn deny(&self, args: &Value, reason: &str) {
        if let Some(scope) = &self.scope {
            audit::denied(scope, &self.path, args, 403, reason);
            self.finish(scope, 403, 0);
        }
    }

    /// The message was served, with this status as an HTTP request would have had, and an
    /// answer of `size` bytes. `size` is only worked out when there is a scope to count it in.
    pub fn done(&self, status: u16, size: impl FnOnce() -> usize) {
        if let Some(scope) = &self.scope {
            self.finish(scope, status, size() as u64);
        }
    }

    /// The connection already holds as many live queries as it may. Counted as throttled.
    pub fn limit_reached(&self) {
        if let Some(scope) = &self.scope {
            scope.mark_throttled();
            self.finish(scope, 429, 0);
        }
    }

    fn finish(&self, scope: &Arc<RequestScope>, status: u16, bytes_out: u64) {
        scope.governance().finish(
            scope,
            &Finished {
                method: "WS",
                path: &self.path,
                query: None,
                class: self.class,
                status,
                bytes_in: self.bytes_in,
                bytes_out,
                elapsed: self.started.elapsed(),
            },
        );
    }
}

/// The length of `value` as JSON, counted without building the text.
pub fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::json_len;

    #[test]
    fn a_length_is_counted_without_building_the_text() {
        for value in [
            json!({"a": [1, 2.5, "\u{e9}"], "b": null}),
            json!([]),
            json!("plain"),
        ] {
            assert_eq!(json_len(&value), value.to_string().len());
        }
    }
}
