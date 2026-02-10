use crate::metrics::Metrics;
use crate::ws::tracker::VoteTracker;
use crate::ws::types::*;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

/// Convert HTTP URL to WebSocket URL
fn http_to_ws_url(http_url: &str) -> String {
    if http_url.starts_with("https://") {
        http_url.replace("https://", "wss://")
    } else if http_url.starts_with("http://") {
        http_url.replace("http://", "ws://")
    } else if http_url.starts_with("wss://") || http_url.starts_with("ws://") {
        http_url.to_string()
    } else {
        format!("wss://{}", http_url)
    }
}

/// Run the vote account subscription loop
pub async fn run_vote_subscription(
    rpc_url: &str,
    vote_pubkey: &str,
    metrics: Arc<Metrics>,
    tracker: Arc<RwLock<VoteTracker>>,
) -> Result<()> {
    let ws_url = http_to_ws_url(rpc_url);
    info!("Starting WebSocket subscription to {}", ws_url);

    loop {
        match subscribe_loop(&ws_url, vote_pubkey, &metrics, &tracker).await {
            Ok(()) => {
                warn!("WebSocket connection closed normally, reconnecting...");
            }
            Err(e) => {
                error!("WebSocket error: {:#}, reconnecting in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn subscribe_loop(
    ws_url: &str,
    vote_pubkey: &str,
    metrics: &Arc<Metrics>,
    tracker: &Arc<RwLock<VoteTracker>>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to WebSocket")?;

    info!("WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    // Subscribe to vote account with jsonParsed encoding and finalized commitment
    let subscribe_msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "accountSubscribe",
        "params": [
            vote_pubkey,
            {
                "encoding": "jsonParsed",
                "commitment": "finalized"
            }
        ]
    });

    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .context("Failed to send subscribe message")?;

    info!("Subscribed to vote account: {}", vote_pubkey);

    let mut subscription_id: Option<u64> = None;

    while let Some(msg) = read.next().await {
        let msg = msg.context("WebSocket receive error")?;

        match msg {
            Message::Text(text) => match serde_json::from_str::<WsMessage>(&text) {
                Ok(WsMessage::SubscriptionResult { result, .. }) => {
                    subscription_id = Some(result);
                    info!("Subscription confirmed, id: {}", result);
                }
                Ok(WsMessage::Notification { params, .. }) => {
                    if let Err(e) = process_notification(&params, metrics, tracker).await {
                        warn!("Error processing notification: {:#}", e);
                    }
                }
                Ok(WsMessage::Error { error, .. }) => {
                    return Err(anyhow!("RPC error {}: {}", error.code, error.message));
                }
                Err(e) => {
                    warn!(
                        "Failed to parse WebSocket message: {}, raw: {}",
                        e,
                        &text[..text.len().min(200)]
                    );
                }
            },
            Message::Ping(data) => {
                write.send(Message::Pong(data)).await?;
            }
            Message::Close(_) => {
                info!("WebSocket closed by server");
                break;
            }
            _ => {}
        }
    }

    if let Some(id) = subscription_id {
        info!("Subscription {} ended", id);
    }

    Ok(())
}

async fn process_notification(
    params: &NotificationParams,
    metrics: &Arc<Metrics>,
    tracker: &Arc<RwLock<VoteTracker>>,
) -> Result<()> {
    let context_slot = params.result.context.slot;
    let value = &params.result.value;

    // Extract parsed vote account data
    let vote_info = match &value.data {
        AccountData::Parsed { parsed, .. } => &parsed.info,
        AccountData::Raw(_, _) => {
            return Err(anyhow!("Expected jsonParsed data, got raw"));
        }
    };

    // Extract votes as (slot, confirmation_count, latency) tuples
    let votes: Vec<(u64, u32, Option<u32>)> = vote_info
        .votes
        .iter()
        .map(|v| (v.slot, v.confirmation_count, v.latency))
        .collect();

    // Get current epoch from epochCredits
    let current_epoch = vote_info
        .epoch_credits
        .last()
        .map(|ec| ec.epoch)
        .unwrap_or(0);

    // Process the update
    let result = {
        let mut tracker = tracker.write().await;
        tracker.process_update(context_slot, &votes, vote_info.root_slot, current_epoch)
    };

    // Update metrics
    update_histogram_metrics(metrics, tracker).await;

    if result.new_votes > 0 || result.missed_count > 0 {
        tracing::debug!(
            "Processed update at slot {}: {} new votes, {} missed",
            context_slot,
            result.new_votes,
            result.missed_count
        );
    }

    Ok(())
}

async fn update_histogram_metrics(metrics: &Arc<Metrics>, tracker: &Arc<RwLock<VoteTracker>>) {
    let tracker = tracker.read().await;

    // Get histograms for each window
    let hist_5m = tracker.window_histogram(300);
    let hist_1h = tracker.window_histogram(3600);
    let hist_epoch = tracker.epoch_histogram();

    // Update count metrics
    for credits in 0..=16u64 {
        let credits_str = credits.to_string();

        metrics
            .vote_credits_histogram_count
            .with_label_values(&["5m", &credits_str])
            .set(hist_5m[credits as usize] as i64);

        metrics
            .vote_credits_histogram_count
            .with_label_values(&["1h", &credits_str])
            .set(hist_1h[credits as usize] as i64);

        metrics
            .vote_credits_histogram_count
            .with_label_values(&["epoch", &credits_str])
            .set(hist_epoch[credits as usize] as i64);
    }

    // Update fraction metrics
    let frac_5m = VoteTracker::histogram_fractions(&hist_5m);
    let frac_1h = VoteTracker::histogram_fractions(&hist_1h);
    let frac_epoch = VoteTracker::histogram_fractions(&hist_epoch);

    for credits in 0..=16u64 {
        let credits_str = credits.to_string();

        metrics
            .vote_credits_histogram_fraction
            .with_label_values(&["5m", &credits_str])
            .set(frac_5m[credits as usize]);

        metrics
            .vote_credits_histogram_fraction
            .with_label_values(&["1h", &credits_str])
            .set(frac_1h[credits as usize]);

        metrics
            .vote_credits_histogram_fraction
            .with_label_values(&["epoch", &credits_str])
            .set(frac_epoch[credits as usize]);
    }
}
