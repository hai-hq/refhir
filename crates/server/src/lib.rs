//! Axum HTTP and the `/sync` websocket. Both call `refhir_fhir::run`.

mod http;
mod sync;

use std::sync::{Arc, RwLock};

use refhir_database::Database;
use refhir_fhir::{Catalog, ConformanceStore};
use tokio::net::TcpListener;

pub use http::{app, AppState};

pub async fn serve(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    listener: TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, app(db, catalog, store)).await
}

#[cfg(test)]
mod tests;
