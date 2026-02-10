use tvc_tracker::config::Args;
use tvc_tracker::logging::init_logging;
use tvc_tracker::metrics::metrics_handler;
use tvc_tracker::poller::run_poll;
use tvc_tracker::rpc::client::HttpRpcClient;
use tvc_tracker::ws::{VoteTracker, run_vote_subscription};

use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;
    println!("tvc_tracker starting with args:\n{:#?}", args);
    let metrics = Arc::new(tvc_tracker::metrics::Metrics::new()?);
    let _log_guard = init_logging(&args.log_dir)?;
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
        axum::serve(listener, app).await.unwrap();
    });

    // Create vote tracker for WebSocket histogram tracking
    let tracker = Arc::new(RwLock::new(VoteTracker::new()));

    // Start WebSocket subscription for per-vote histogram metrics
    let ws_metrics = metrics.clone();
    let ws_tracker = tracker.clone();
    let ws_rpc_url = args.rpc_url.clone();
    let ws_vote_pubkey = args.vote_pubkey.clone();
    tokio::spawn(async move {
        if let Err(e) =
            run_vote_subscription(&ws_rpc_url, &ws_vote_pubkey, ws_metrics, ws_tracker).await
        {
            tracing::error!("WebSocket subscription failed: {:#}", e);
        }
    });

    // Continue with REST polling for aggregate metrics
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(60))
        .user_agent("tvc_tracker/0.1")
        .build()?;

    let rpc = HttpRpcClient {
        http,
        rpc_url: args.rpc_url.clone(),
    };
    run_poll(&rpc, &args, metrics.clone()).await?;
    Ok(())
}
