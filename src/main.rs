use tvc_tracker::config::Args;
use tvc_tracker::logging::init_logging;
use tvc_tracker::metrics::metrics_handler;
use tvc_tracker::poller::run_poll;
use tvc_tracker::rpc::client::HttpRpcClient;

use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;
    println!("tvc_tracker starting with args:\n{:#?}", args);
    let metrics = std::sync::Arc::new(tvc_tracker::metrics::Metrics::new()?);
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
