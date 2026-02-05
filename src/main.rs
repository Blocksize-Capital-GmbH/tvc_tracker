mod metrics;
use crate::metrics::MAX_CREDITS_PER_SLOT;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::RETRY_AFTER;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Vote account pubkey (base58)
    #[arg(long)]
    vote_pubkey: String,

    /// HTTP RPC URL (public endpoints are rate-limited)
    #[arg(long, default_value = "https://api.mainnet.solana.com")]
    rpc_url: String,

    /// WebSocket URL (used only in ws-trigger mode)
    #[arg(long, default_value = "wss://api.mainnet.solana.com")]
    ws_url: String,

    /// Commitment: finalized is closest to "rooted" accounting
    #[arg(long, default_value = "finalized")]
    commitment: Commitment,

    /// Poll interval seconds (poll mode)
    #[arg(long, default_value_t = 15)]
    interval_secs: u64,

    /// Mode: poll (simple) or ws-trigger (subscribe then fetch)
    #[arg(long, default_value = "poll")]
    mode: Mode,

    /// Directory to write logs to
    #[arg(long, default_value = "logs")]
    log_dir: String,

    /// Port to serve metrics on
    #[arg(long, default_value_t = 7999)]
    metrics_port: u16,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Poll,
    WsTrigger,
}

#[derive(Copy, Clone, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Serialize)]
struct RpcRequest<'a, T> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: T,
}

#[derive(Deserialize, Debug)]
struct RpcResponse<T> {
    jsonrpc: String,
    id: u64,
    result: Option<T>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize, Debug)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize, Debug)]
struct GetVoteAccountsResult {
    current: Vec<VoteAccount>,
    delinquent: Vec<VoteAccount>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct VoteAccount {
    vote_pubkey: String,
    node_pubkey: String,
    activated_stake: u64,
    commission: u8,
    epoch_credits: Vec<[u64; 3]>, // [epoch, credits, prevCredits]
    last_vote: u64,
    root_slot: u64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EpochInfo {
    epoch: u64,
    slot_index: u64,
    slots_in_epoch: u64,
    absolute_slot: u64,
}

#[derive(Debug, Clone)]
struct CreditsSnapshot {
    epoch: u64,
    credits_total: u64,
    credits_at_epoch_start: u64,
    credits_this_epoch: u64,
    root_slot: u64,
}

fn snapshot_from_vote_account(acct: &VoteAccount) -> anyhow::Result<CreditsSnapshot> {
    let last = acct
        .epoch_credits
        .last()
        .ok_or_else(|| anyhow!("epochCredits empty"))?;

    let epoch = last[0];
    let credits_total = last[1];
    let credits_at_epoch_start = last[2];
    let credits_this_epoch = credits_total.saturating_sub(credits_at_epoch_start);

    Ok(CreditsSnapshot {
        epoch,
        credits_total,
        credits_at_epoch_start,
        credits_this_epoch,
        root_slot: acct.root_slot,
    })
}

async fn rpc_post_json(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut attempt: u32 = 0;
    let mut backoff = Duration::from_millis(250);
    let max_attempts = 6;

    loop {
        attempt += 1;

        let resp = client.post(url).json(&body).send().await;

        match resp {
            Ok(resp) => {
                // Handle 429 rate limiting
                if resp.status() == 429 {
                    let wait = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(1);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }

                // Handle 5xx with retry
                if resp.status().is_server_error() {
                    if attempt >= max_attempts {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        return Err(anyhow!("RPC {url} server error {status}: {text}"));
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
                    continue;
                }

                let status = resp.status();
                let v: serde_json::Value = resp.json().await.context("failed to parse JSON")?;
                if !status.is_success() {
                    return Err(anyhow!("HTTP status {status}: {v}"));
                }
                return Ok(v);
            }

            Err(e) => {
                // Retry only on transient-ish failures
                let retryable = e.is_timeout() || e.is_connect() || e.is_request();
                if !retryable || attempt >= max_attempts {
                    return Err(anyhow!("HTTP POST to {url} failed").context(e));
                }
                tracing::warn!(
                    "RPC request failed (attempt {attempt}/{max_attempts}): {e}. Retrying in {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
            }
        }
    }
}

async fn fetch_vote_account(
    client: &reqwest::Client,
    rpc_url: &str,
    vote_pubkey: &str,
    commitment: Commitment,
) -> Result<VoteAccount> {
    // getVoteAccounts supports a votePubkey filter. :contentReference[oaicite:4]{index=4}
    const REQ_ID: u64 = 1;
    let req = RpcRequest {
        jsonrpc: "2.0",
        id: REQ_ID,
        method: "getVoteAccounts",
        params: vec![json!({
            "commitment": commitment,    // "finalized" is safest for rooted accounting
            "votePubkey": vote_pubkey
        })],
    };

    let body = serde_json::to_value(req)?;
    let raw = rpc_post_json(client, rpc_url, body).await?;

    let parsed: RpcResponse<GetVoteAccountsResult> = serde_json::from_value(raw)?;
    // actually use these fields so dead_code shuts up
    if parsed.jsonrpc != "2.0" {
        return Err(anyhow!("unexpected jsonrpc version: {}", parsed.jsonrpc));
    }
    if parsed.id != REQ_ID {
        return Err(anyhow!("mismatched response id: {}", parsed.id));
    }

    if let Some(e) = parsed.error {
        return Err(anyhow!("RPC error {}: {}", e.code, e.message));
    }
    let result = parsed
        .result
        .ok_or_else(|| anyhow!("missing result field"))?;

    // Entry could theoretically be in current or delinquent.
    let acct = result
        .current
        .into_iter()
        .chain(result.delinquent.into_iter())
        .find(|a| a.vote_pubkey == vote_pubkey)
        .ok_or_else(|| anyhow!("vote pubkey not found in getVoteAccounts response"))?;

    info!(
        "node={} stake={} commission={} last_vote={} root_slot={}",
        acct.node_pubkey, acct.activated_stake, acct.commission, acct.last_vote, acct.root_slot
    );

    Ok(acct.clone())
}

async fn fetch_epoch_info(
    client: &reqwest::Client,
    rpc_url: &str,
    commitment: Commitment,
) -> Result<EpochInfo> {
    let req = RpcRequest {
        jsonrpc: "2.0",
        id: 2,
        method: "getEpochInfo",
        params: vec![json!({ "commitment": commitment })],
    };

    let body = serde_json::to_value(req)?;
    let raw = rpc_post_json(client, rpc_url, body).await?;
    let parsed: RpcResponse<EpochInfo> = serde_json::from_value(raw)?;

    if let Some(e) = parsed.error {
        return Err(anyhow!("RPC error {}: {}", e.code, e.message));
    }
    parsed.result.ok_or_else(|| anyhow!("missing result"))
}

fn append_log(prev: Option<(&CreditsSnapshot, Instant)>, cur: &CreditsSnapshot, now: Instant) {
    if let Some((p, t0)) = prev {
        let d_credits = cur.credits_total.saturating_sub(p.credits_total);
        let dt = now.duration_since(t0).as_secs_f64().max(0.001);
        let rate = (d_credits as f64) / dt;
        info!(
            "epoch={} credits_this_epoch={} (Δtotal={} | {:.2} credits/s)",
            cur.epoch, cur.credits_this_epoch, d_credits, rate
        );
    } else {
        info!(
            "epoch={} credits_this_epoch={} (total={} start={})",
            cur.epoch, cur.credits_this_epoch, cur.credits_total, cur.credits_at_epoch_start
        );
    }
}

fn init_logging(log_dir: &str) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    // daily rotating file: logs/tvc_tracker.YYYY-MM-DD
    let file_appender = tracing_appender::rolling::daily(log_dir, "tvc_tracker.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false) // log files don’t need rainbow control codes
        .init();

    Ok(guard) // keep this alive or logs may not flush
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    println!("tvc_tracker starting with args:\n{:#?}", args);
    let metrics = std::sync::Arc::new(metrics::Metrics::new()?);
    let _log_guard = init_logging(&args.log_dir)?;
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get({
            let metrics = metrics.clone();
            move || metrics::metrics_handler(metrics.clone())
        }),
    );

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.metrics_port);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(60))
        .user_agent("tvc_tracker/0.1")
        .build()?;

    match args.mode {
        Mode::Poll => run_poll(&client, &args, metrics.clone()).await,
        Mode::WsTrigger => run_ws_trigger(&client, &args).await,
    }
}

fn window_delta(
    hist: &VecDeque<(Instant, u64)>,
    now: Instant,
    window: Duration,
    current: u64,
) -> u64 {
    let start = now - window;
    // pick the oldest sample with timestamp >= start; if none, use oldest available
    let base = hist
        .iter()
        .find(|(t, _)| *t >= start)
        .or_else(|| hist.front())
        .map(|(_, v)| *v)
        .unwrap_or(current);
    current.saturating_sub(base)
}

async fn run_poll(
    client: &reqwest::Client,
    args: &Args,
    m: std::sync::Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let mut prev: Option<(CreditsSnapshot, Instant)> = None;
    let mut prev_missed: Option<u64> = None;
    let mut missed_total_acc: u64 = 0;
    let mut hist: VecDeque<(Instant, u64)> = VecDeque::new(); // (time, missed_total_acc)

    loop {
        let res = async {
            let epoch_info = fetch_epoch_info(client, &args.rpc_url, args.commitment).await?;
            let acct =
                fetch_vote_account(client, &args.rpc_url, &args.vote_pubkey, args.commitment)
                    .await?;
            let cur = snapshot_from_vote_account(&acct)?;
            Ok::<_, anyhow::Error>((epoch_info, acct, cur))
        }
        .await;

        match res {
            Ok((epoch_info, acct, cur)) => {
                // normal metrics updates
                m.rpc_up.set(1);
                m.rpc_last_success.set(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                );

                // get date & set time
                let now = Instant::now();
                let acct =
                    fetch_vote_account(client, &args.rpc_url, &args.vote_pubkey, args.commitment)
                        .await?;

                // compute metrics
                let cur = snapshot_from_vote_account(&acct)?;
                let epoch_info = fetch_epoch_info(client, &args.rpc_url, args.commitment).await?;
                let last_epoch = epoch_info.epoch.saturating_sub(1);
                let slots_elapsed = epoch_info.slot_index.saturating_add(1);
                let expected_max = slots_elapsed.saturating_mul(MAX_CREDITS_PER_SLOT);
                let epoch_start_slot = epoch_info
                    .absolute_slot
                    .saturating_sub(epoch_info.slot_index);

                let rooted_slots_elapsed = acct
                    .root_slot
                    .saturating_sub(epoch_start_slot)
                    .saturating_add(1);

                let expected_max_rooted = rooted_slots_elapsed.saturating_mul(MAX_CREDITS_PER_SLOT);

                // actual credits this epoch from epochCredits last tuple
                let last = acct
                    .epoch_credits
                    .last()
                    .ok_or_else(|| anyhow!("epochCredits empty"))?;

                let credits_this_epoch = last[1].saturating_sub(last[2]);

                let missed_now = expected_max_rooted.saturating_sub(credits_this_epoch);

                let delta_missed = match prev_missed {
                    Some(p) => missed_now.saturating_sub(p),
                    None => 0,
                };
                prev_missed = Some(missed_now);

                if let Some(t) = acct.epoch_credits.iter().find(|t| t[0] == last_epoch) {
                    let earned_last = t[1].saturating_sub(t[2]);
                    let expected_last = epoch_info
                        .slots_in_epoch
                        .saturating_mul(MAX_CREDITS_PER_SLOT);
                    let missed_last = expected_last.saturating_sub(earned_last);
                    m.missed_last_epoch.set(missed_last as i64);
                }

                m.missed_since_last_poll.set(delta_missed as i64);

                if delta_missed > 0 {
                    m.missed_total.inc_by(delta_missed);
                    missed_total_acc = missed_total_acc.saturating_add(delta_missed);
                }

                hist.push_back((now, missed_total_acc));

                let cutoff = now - Duration::from_secs(3600);
                while let Some((t, _)) = hist.front() {
                    if *t < cutoff {
                        hist.pop_front();
                    } else {
                        break;
                    }
                }

                let missed_5m =
                    window_delta(&hist, now, Duration::from_secs(300), missed_total_acc);
                let missed_1h =
                    window_delta(&hist, now, Duration::from_secs(3600), missed_total_acc);

                m.missed_5m.set(missed_5m as i64);
                m.missed_1h.set(missed_1h as i64);
                m.expected_max.set(expected_max as i64);
                m.actual.set(cur.credits_this_epoch as i64);
                m.missed_current_epoch.set(missed_now as i64);

                append_log(prev.as_ref().map(|(s, t)| (s, *t)), &cur, now);
                prev = Some((cur, now));

                sleep(Duration::from_secs(args.interval_secs)).await;
            } // OK
            Err(e) => {
                m.rpc_up.set(0);
                m.rpc_errors.inc();
                tracing::warn!("poll failed: {e:#}");
                // IMPORTANT: do not touch “prev_missed” / window state on failed poll
            }
        } // match
    } //loop
}

async fn run_ws_trigger(client: &reqwest::Client, args: &Args) -> Result<()> {
    // WebSocket accountSubscribe: notify on vote account changes, then fetch authoritative credits via getVoteAccounts.
    // accountSubscribe config supports commitment + encoding. :contentReference[oaicite:5]{index=5}
    let (ws_stream, _) = tokio_tungstenite::connect_async(&args.ws_url)
        .await
        .with_context(|| format!("failed to connect websocket {}", args.ws_url))?;

    let (mut ws_write, mut ws_read) = ws_stream.split();

    let sub_msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "accountSubscribe",
        "params": [
            args.vote_pubkey,
            { "commitment": args.commitment, "encoding": "base64" }
        ]
    });
    ws_write.send(sub_msg.to_string().into()).await?;

    let mut prev: Option<(CreditsSnapshot, Instant)> = None;

    while let Some(msg) = ws_read.next().await {
        let msg = msg?;
        if !msg.is_text() {
            continue;
        }

        // Any account notification is our trigger. We ignore payload details and re-fetch via getVoteAccounts.
        let now = Instant::now();
        let acct =
            fetch_vote_account(client, &args.rpc_url, &args.vote_pubkey, args.commitment).await?;
        let cur = snapshot_from_vote_account(&acct)?;

        append_log(prev.as_ref().map(|(s, t)| (s, *t)), &cur, now);
        prev = Some((cur, now));
    }

    Err(anyhow!("websocket closed"))
}
