//! Axum HTTP and the `/sync` websocket. Both call `refhir_fhir::run`.

mod auth;
mod cluster;
mod governance;
mod graphql;
mod health;
mod http;
mod jobs;
mod sync;
mod xml;

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use refhir_database::Database;
use refhir_fhir::{Catalog, ConformanceStore};
use tokio::net::TcpListener;

pub use auth::{FhirUserLookup, OidcConfig, SmartStore};
pub use cluster::{bootstrap_router, serve_bootstrap, Bootstrap};
pub use governance::{
    AuditConfig, Cidr, ClientRate, Costs, Governance, GovernanceConfig, ProxyConfig, QuotaConfig,
    QuotaLimits, RateLimitConfig, RequestLogConfig,
};
pub use http::{
    app, app_parts, app_with, AppState, ServerConfig, SharedGovernanceStore, DEFAULT_PAGE_SIZE,
};
pub use jobs::SharedJobStore;

/// How long the rows still buffered are given to reach the database once the server is told to
/// stop. Under the ten seconds `deploy/production.md` asks a pod's termination grace to be.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(8);

/// Serve until the process is asked to stop (SIGINT or SIGTERM), then write the usage, request
/// log and audit rows that are still buffered. The server is stopped as it always was, by
/// ending it, but without losing the last moments of the audit trail. A second signal, or a
/// database that does not take the rows within [`DRAIN_TIMEOUT`], ends the process at once.
///
/// The address a caller with no token is told apart by is the connection's, which this provides.
/// A router from [`app`] or [`app_with`] served any other way has none, and its anonymous callers
/// share one limit: serve it with `into_make_service_with_connect_info::<SocketAddr>()`.
pub async fn serve(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    listener: TcpListener,
    config: ServerConfig,
) -> std::io::Result<()> {
    serve_until(
        db,
        catalog,
        store,
        listener,
        config,
        shutdown_signal(),
        shutdown_signal(),
    )
    .await
}

/// [`serve`], stopping when `shutdown` completes and giving up on the buffered rows when `force`
/// does.
async fn serve_until(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    listener: TcpListener,
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()>,
    force: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    let (router, governance) = app_parts(db, catalog, store, config);
    // The connection's address is what a caller with no token is told apart by.
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        result = server.into_future() => result,
        _ = shutdown => {
            tracing::info!("stopping; writing buffered usage, request log and audit rows");
            let drain = async {
                governance.flush().await;
                governance.leave().await;
            };
            // The write is a blocking task that nothing can cancel, and the runtime waits for
            // it on its way out. So when the database does not answer, the process has to end
            // here, and tokio keeps the signal handlers it installed, so a second Ctrl-C would
            // otherwise do nothing at all.
            tokio::select! {
                drained = tokio::time::timeout(DRAIN_TIMEOUT, drain) => {
                    if drained.is_err() {
                        give_up("the buffered rows were not written within the time allowed");
                    }
                }
                _ = force => give_up("told to stop a second time"),
            }
            Ok(())
        }
    }
}

/// End the process without the rows that were not written.
fn give_up(why: &str) -> ! {
    tracing::error!("{why}; stopping without them");
    std::process::exit(1)
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod billing_tests;
#[cfg(test)]
mod booking_tests;
#[cfg(test)]
mod tests;
