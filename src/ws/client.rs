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

    // Get current epoch info from epochCredits
    let current_epoch_entry = vote_info.epoch_credits.last();
    let current_epoch = current_epoch_entry.map(|ec| ec.epoch).unwrap_or(0);

    // Get credits earned THIS epoch (credits - previous_credits)
    let epoch_credits = current_epoch_entry
        .map(|ec| ec.credits.saturating_sub(ec.previous_credits))
        .unwrap_or(0);

    // Process the update
    let result = {
        let mut tracker = tracker.write().await;
        tracker.process_update(
            context_slot,
            &votes,
            vote_info.root_slot,
            current_epoch,
            epoch_credits,
        )
    };

    // Update metrics
    update_histogram_metrics(metrics, tracker).await;

    if result.new_votes > 0 || result.missed_credits > 0 {
        tracing::debug!(
            "Processed update at slot {}: {} new votes, {} missed credits",
            context_slot,
            result.new_votes,
            result.missed_credits
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

    // Get missed credits for each window
    let missed_5m = tracker.window_missed(300);
    let missed_1h = tracker.window_missed(3600);
    let _missed_epoch = tracker.epoch_missed(); // Epoch data comes from HTTP poller

    // Update histogram count metrics
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

    // Update histogram fraction metrics
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

    // Update missed_vote_credits from WebSocket tracking
    // This uses epoch_credits as source of truth for consistency
    metrics.missed_5m.set(missed_5m as i64);
    metrics.missed_1h.set(missed_1h as i64);

    // Calculate efficiency: (total_slots * 16 - missed) / (total_slots * 16)
    // Total votes = histogram total, expected_credits = total_votes * 16
    let total_votes_5m = VoteTracker::histogram_total(&hist_5m);
    let total_votes_1h = VoteTracker::histogram_total(&hist_1h);

    // For efficiency, we need expected_max which includes missed slots
    // expected_credits = histogram_credits + missed_credits
    // efficiency = histogram_credits / expected_credits
    let hist_credits_5m = VoteTracker::histogram_credits(&hist_5m);
    let hist_credits_1h = VoteTracker::histogram_credits(&hist_1h);

    let expected_5m = hist_credits_5m + missed_5m;
    let expected_1h = hist_credits_1h + missed_1h;

    if expected_5m > 0 {
        let eff_5m = hist_credits_5m as f64 / expected_5m as f64;
        let avg_credits_5m = if total_votes_5m > 0 {
            hist_credits_5m as f64 / total_votes_5m as f64
        } else {
            0.0
        };
        metrics.vote_credits_efficiency_5m.set(eff_5m);
        metrics.vote_credits_per_slot_5m.set(avg_credits_5m);
        metrics.vote_latency_slots_5m.set(17.0 - avg_credits_5m);
    }

    if expected_1h > 0 {
        let eff_1h = hist_credits_1h as f64 / expected_1h as f64;
        let avg_credits_1h = if total_votes_1h > 0 {
            hist_credits_1h as f64 / total_votes_1h as f64
        } else {
            0.0
        };
        metrics.vote_credits_efficiency_1h.set(eff_1h);
        metrics.vote_credits_per_slot_1h.set(avg_credits_1h);
        metrics.vote_latency_slots_1h.set(17.0 - avg_credits_1h);
    }
}
