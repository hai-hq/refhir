//! Axum HTTP and the `/sync` websocket. Both call `refhir_fhir::run`.

mod auth;
mod cluster;
mod graphql;
mod health;
mod http;
mod jobs;
mod sync;
mod xml;

use std::sync::{Arc, RwLock};

use refhir_database::Database;
use refhir_fhir::{Catalog, ConformanceStore};
use tokio::net::TcpListener;

pub use auth::{FhirUserLookup, OidcConfig, SmartStore};
pub use cluster::{bootstrap_router, serve_bootstrap, Bootstrap};
pub use http::{app, app_with, AppState, ServerConfig};
pub use jobs::SharedJobStore;

pub async fn serve(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    listener: TcpListener,
    config: ServerConfig,
) -> std::io::Result<()> {
    axum::serve(listener, app_with(db, catalog, store, config)).await
}

#[cfg(test)]
mod tests;
