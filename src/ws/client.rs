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
                metrics.ws_connected.set(0);
            }
            Err(e) => {
                error!("WebSocket error: {:#}, reconnecting in 5s...", e);
                metrics.ws_connected.set(0);
                metrics.ws_errors.inc();
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
    metrics.ws_connected.set(1);

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
                    } else {
                        // Update last successful message timestamp
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        metrics.ws_last_message.set(now);
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

    // Get credits info from epochCredits
    let current_epoch_entry = vote_info.epoch_credits.last();

    // Credits earned THIS epoch = credits - previous_credits
    // This is what `solana vote-account` shows for the current epoch
    let epoch_credits = current_epoch_entry
        .map(|ec| ec.credits.saturating_sub(ec.previous_credits))
        .unwrap_or(0);

    // Get epoch directly from epochCredits (more accurate than calculating from root_slot)
    let epoch = current_epoch_entry.map(|ec| ec.epoch);

    // Process the update
    let result = {
        let mut tracker = tracker.write().await;
        tracker.process_update(
            context_slot,
            &votes,
            vote_info.root_slot,
            epoch_credits,
            epoch,
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
    let missed_epoch = tracker.epoch_missed();
    metrics.missed_5m.set(missed_5m as i64);
    metrics.missed_1h.set(missed_1h as i64);
    metrics.missed_current_epoch.set(missed_epoch as i64);

    // Calculate totals and credits from histograms
    let total_votes_5m = VoteTracker::histogram_total(&hist_5m);
    let total_votes_1h = VoteTracker::histogram_total(&hist_1h);
    let total_votes_epoch = VoteTracker::histogram_total(&hist_epoch);

    let hist_credits_5m = VoteTracker::histogram_credits(&hist_5m);
    let hist_credits_1h = VoteTracker::histogram_credits(&hist_1h);
    let hist_credits_epoch = VoteTracker::histogram_credits(&hist_epoch);

    // For efficiency: expected_credits = histogram_credits + missed_credits
    // efficiency = histogram_credits / expected_credits
    let expected_5m = hist_credits_5m + missed_5m;
    let expected_1h = hist_credits_1h + missed_1h;
    let expected_epoch = hist_credits_epoch + missed_epoch;

    // Get epoch info for projections
    let epoch_info = tracker.epoch_info();
    let slots_in_epoch = epoch_info.map(|e| e.slots_in_epoch).unwrap_or(432_000);
    let current_epoch_credits = tracker.current_epoch_credits();

    // Calculate remaining slots in epoch
    let slots_elapsed = epoch_info.map(|e| e.slot_index + 1).unwrap_or(1);
    let remaining_slots = slots_in_epoch.saturating_sub(slots_elapsed);

    // 5-minute metrics
    let avg_credits_5m = if total_votes_5m > 0 {
        hist_credits_5m as f64 / total_votes_5m as f64
    } else {
        0.0
    };

    if expected_5m > 0 {
        let eff_5m = hist_credits_5m as f64 / expected_5m as f64;
        metrics.vote_credits_efficiency_5m.set(eff_5m);
        metrics.vote_credits_per_slot_5m.set(avg_credits_5m);
        // Latency: credits = 17 - latency, so latency = 17 - credits
        // Latency 1 = 16 credits (fastest), Latency 17 = 0 credits
        metrics.vote_latency_slots_5m.set(17.0 - avg_credits_5m);
    }

    // Projected credits at epoch end: actual + (remaining_slots × 5m_rate)
    let projected_5m =
        current_epoch_credits as i64 + (avg_credits_5m * remaining_slots as f64) as i64;
    metrics.projected_credits_5m.set(projected_5m);

    // 1-hour metrics
    let avg_credits_1h = if total_votes_1h > 0 {
        hist_credits_1h as f64 / total_votes_1h as f64
    } else {
        0.0
    };

    if expected_1h > 0 {
        let eff_1h = hist_credits_1h as f64 / expected_1h as f64;
        metrics.vote_credits_efficiency_1h.set(eff_1h);
        metrics.vote_credits_per_slot_1h.set(avg_credits_1h);
        metrics.vote_latency_slots_1h.set(17.0 - avg_credits_1h);
    }

    // Projected credits at epoch end: actual + (remaining_slots × 1h_rate)
    let projected_1h =
        current_epoch_credits as i64 + (avg_credits_1h * remaining_slots as f64) as i64;
    metrics.projected_credits_1h.set(projected_1h);

    // Epoch metrics (now from WebSocket, not HTTP)
    if expected_epoch > 0 {
        let eff_epoch = hist_credits_epoch as f64 / expected_epoch as f64;
        let avg_credits_epoch = if total_votes_epoch > 0 {
            hist_credits_epoch as f64 / total_votes_epoch as f64
        } else {
            0.0
        };
        metrics.vote_credits_efficiency_epoch.set(eff_epoch);
        metrics.vote_credits_per_slot_epoch.set(avg_credits_epoch);
        metrics
            .vote_latency_slots_epoch
            .set(17.0 - avg_credits_epoch);
    }

    // Set epoch info metrics
    if let Some(epoch_info) = tracker.epoch_info() {
        metrics.epoch.set(epoch_info.epoch as i64);
        metrics.slot_index.set(epoch_info.slot_index as i64);

        // Current epoch credits from vote account (credits - previous_credits)
        // This matches what `solana vote-account` shows for the current epoch
        metrics
            .total_epoch_credits
            .set(current_epoch_credits as i64);

        // Maximum possible credits this epoch (slots_in_epoch × 16)
        let epoch_max = epoch_info.slots_in_epoch * 16;
        metrics.epoch_expected_max.set(epoch_max as i64);

        // Actual and expected from our tracking (since tracker started)
        metrics.actual.set(hist_credits_epoch as i64);
        metrics.expected_max.set(expected_epoch as i64);
    }
}
