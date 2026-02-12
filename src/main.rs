use tvc_tracker::config::Args;
use tvc_tracker::logging::init_logging;
use tvc_tracker::metrics::metrics_handler;
use tvc_tracker::ws::{VoteTracker, run_vote_subscription};

use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::RwLock;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;
    println!("tvc_tracker v{VERSION} starting with args:\n{:#?}", args);

    let metrics = Arc::new(tvc_tracker::metrics::Metrics::new()?);
    let _log_guard = init_logging(&args.log_dir)?;

    // Set up metrics HTTP server
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get({
            let metrics = metrics.clone();
            move || metrics_handler(metrics.clone())
        }),
    );

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.metrics_port);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tracing::info!("Metrics server listening on {}", addr);
        axum::serve(listener, app).await.unwrap();
    });

    // Create vote tracker for WebSocket histogram tracking
    let tracker = Arc::new(RwLock::new(VoteTracker::new()));

    // Start WebSocket subscription - this is now the primary data source
    // All metrics are derived from WebSocket updates
    // Epoch info is calculated from slot numbers (no HTTP needed)
    tracing::info!(
        "Starting WebSocket subscription for vote account {}",
        args.vote_pubkey
    );

    // Run WebSocket subscription with automatic reconnection
    run_vote_subscription(&args.rpc_url, &args.vote_pubkey, metrics, tracker).await?;

    Ok(())
}
