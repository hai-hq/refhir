//! Axum HTTP and the `/sync` websocket. Both call `haifhir_fhir::run`.

mod http;
mod sync;

use std::sync::{Arc, RwLock};

use haifhir_database::Database;
use haifhir_fhir::Catalog;
use tokio::net::TcpListener;

pub use http::{app, AppState};

pub async fn serve(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    listener: TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, app(db, catalog)).await
}

#[cfg(test)]
mod tests;
