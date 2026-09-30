//! Axum HTTP and the `/sync` websocket. Both call `haifhir_fhir::run`.

mod http;
mod sync;

use std::sync::Arc;

use haifhir_database::Database;
use tokio::net::TcpListener;

pub use http::{app, AppState};

pub async fn serve(db: Arc<Database>, listener: TcpListener) -> std::io::Result<()> {
    axum::serve(listener, app(db)).await
}

#[cfg(test)]
mod tests;
