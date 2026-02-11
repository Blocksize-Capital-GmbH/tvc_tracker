use crate::config::Args;
use crate::logging::append_log;
use crate::metrics::{MAX_CREDITS_PER_SLOT, Metrics};
use crate::rpc::client::RpcClient;
use crate::rpc::types::VoteAccount;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct CreditsSnapshot {
    pub epoch: u64,
    pub credits_total: u64,
    pub credits_at_epoch_start: u64,
    pub credits_this_epoch: u64,
}

/// History entry: (timestamp, cumulative_missed, root_slot)
type HistEntry = (Instant, u64, u64);

#[derive(Debug)]
pub struct PollState {
    pub prev: Option<(CreditsSnapshot, Instant)>,
    pub prev_missed: Option<u64>,
    pub prev_epoch: Option<u64>,
    pub missed_total_acc: u64,
    pub hist: VecDeque<HistEntry>,
    pub base_interval: Duration,
    pub backoff: Duration,
}

impl PollState {
    pub fn new(interval_secs: u64) -> Self {
        let base = Duration::from_secs(interval_secs);
        Self {
            prev: None,
            prev_missed: None,
            prev_epoch: None,
            missed_total_acc: 0,
            hist: VecDeque::new(),
            base_interval: base,
            backoff: base,
        }
    }

    pub fn on_success(&mut self) {
        self.backoff = self.base_interval
    }

    pub fn on_error(&mut self) {
        let next = self.backoff.saturating_mul(2);
        self.backoff = std::cmp::min(next, Duration::from_secs(600));
    }
}

pub async fn poll_once<C: RpcClient + Sync>(
    rpc: &C,
    args: &Args,
    m: &std::sync::Arc<Metrics>,
    state: &mut PollState,
) -> Result<()> {
    let epoch_info = rpc.get_epoch_info(args.commitment).await?;
    let acct = rpc
        .get_vote_account(&args.vote_pubkey, args.commitment)
        .await?;
    let cur = snapshot_from_vote_account(&acct)?;

    // Note: HTTP poller is deprecated - WebSocket is now the primary data source
    // These ws_ metrics are shared between both, set them here for backwards compatibility
    m.ws_connected.set(1);
    m.ws_last_message.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    );

    let now = Instant::now();

    let epoch_start_slot = epoch_info
        .absolute_slot
        .saturating_sub(epoch_info.slot_index);

    let rooted_slots_elapsed = acct
        .root_slot
        .saturating_sub(epoch_start_slot)
        .saturating_add(1);

    let expected_max_rooted = rooted_slots_elapsed.saturating_mul(MAX_CREDITS_PER_SLOT);

    // actual credits this epoch from epochCredits
    // Find the entry for the current epoch (don't assume last entry is current)
    let current_epoch_credits = acct
        .epoch_credits
        .iter()
        .find(|e| e[0] == epoch_info.epoch)
        .or_else(|| acct.epoch_credits.last())
        .ok_or_else(|| anyhow!("epochCredits empty"))?;

    let credits_this_epoch = current_epoch_credits[1].saturating_sub(current_epoch_credits[2]);

    let missed_now = expected_max_rooted.saturating_sub(credits_this_epoch);

    // Detect epoch boundary: reset prev_missed when epoch changes
    let epoch_changed = state
        .prev_epoch
        .map(|e| e != epoch_info.epoch)
        .unwrap_or(false);

    let delta_missed = if epoch_changed {
        // Epoch changed: start fresh, count from 0 for new epoch
        0
    } else {
        match state.prev_missed {
            Some(p) => missed_now.saturating_sub(p),
            None => 0,
        }
    };

    state.prev_missed = Some(missed_now);
    state.prev_epoch = Some(epoch_info.epoch);
    let last_epoch = epoch_info.epoch.saturating_sub(1);

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
        state.missed_total_acc = state.missed_total_acc.saturating_add(delta_missed);
    }

    // Track history with slot for efficiency calculation
    state
        .hist
        .push_back((now, state.missed_total_acc, acct.root_slot));

    let cutoff = now - Duration::from_secs(3600);
    while let Some((t, _, _)) = state.hist.front() {
        if *t < cutoff {
            state.hist.pop_front();
        } else {
            break;
        }
    }

    let stats_5m = window_stats(
        &state.hist,
        now,
        Duration::from_secs(300),
        state.missed_total_acc,
        acct.root_slot,
    );
    let stats_1h = window_stats(
        &state.hist,
        now,
        Duration::from_secs(3600),
        state.missed_total_acc,
        acct.root_slot,
    );

    // Calculate rates (missed credits per minute)
    let rate_5m = if stats_5m.elapsed.as_secs() > 0 {
        (stats_5m.missed as f64) / (stats_5m.elapsed.as_secs_f64() / 60.0)
    } else {
        0.0
    };
    let rate_1h = if stats_1h.elapsed.as_secs() > 0 {
        (stats_1h.missed as f64) / (stats_1h.elapsed.as_secs_f64() / 60.0)
    } else {
        0.0
    };

    // Calculate efficiency (fraction of max credits earned in window)
    // efficiency = earned / expected = (expected - missed) / expected = 1 - (missed / expected)
    let efficiency_5m = if stats_5m.expected_max > 0 {
        1.0 - (stats_5m.missed as f64 / stats_5m.expected_max as f64)
    } else {
        1.0
    };
    let efficiency_1h = if stats_1h.expected_max > 0 {
        1.0 - (stats_1h.missed as f64 / stats_1h.expected_max as f64)
    } else {
        1.0
    };

    // Calculate epoch-level efficiency (no extrapolation, just actual vs expected)
    let efficiency_epoch = if expected_max_rooted > 0 {
        credits_this_epoch as f64 / expected_max_rooted as f64
    } else {
        1.0
    };

    // Calculate credits per slot for each window
    // credits_per_slot = (expected - missed) / slots = efficiency * MAX_CREDITS_PER_SLOT
    let credits_per_slot_5m = efficiency_5m * MAX_CREDITS_PER_SLOT as f64;
    let credits_per_slot_1h = efficiency_1h * MAX_CREDITS_PER_SLOT as f64;
    let credits_per_slot_epoch = efficiency_epoch * MAX_CREDITS_PER_SLOT as f64;

    // Calculate implied vote latency in slots
    // In TVC: 16 credits = 1 slot latency, 15 = 2 slots, ..., 1 = 16 slots, 0 = missed
    // latency = 17 - credits_per_slot (clamped to valid range)
    // Note: This is an "implied" latency that includes the effect of missed votes
    let latency_5m = (17.0 - credits_per_slot_5m).clamp(1.0, 17.0);
    let latency_1h = (17.0 - credits_per_slot_1h).clamp(1.0, 17.0);
    let latency_epoch = (17.0 - credits_per_slot_epoch).clamp(1.0, 17.0);

    // Calculate projected credits for the epoch
    // Based on actual credits earned per rooted slot, extrapolated to full epoch
    // Use rooted_slots_elapsed (not slot_index) because credits are earned on rooted slots
    let credits_per_slot = if rooted_slots_elapsed > 0 {
        credits_this_epoch as f64 / rooted_slots_elapsed as f64
    } else {
        0.0
    };
    let projected_credits = (credits_per_slot * epoch_info.slots_in_epoch as f64) as i64;

    m.missed_5m.set(stats_5m.missed as i64);
    m.missed_1h.set(stats_1h.missed as i64);
    m.missed_rate_5m.set(rate_5m);
    m.missed_rate_1h.set(rate_1h);
    m.vote_credits_efficiency_5m.set(efficiency_5m);
    m.vote_credits_efficiency_1h.set(efficiency_1h);
    m.vote_credits_efficiency_epoch.set(efficiency_epoch);
    m.vote_credits_per_slot_5m.set(credits_per_slot_5m);
    m.vote_credits_per_slot_1h.set(credits_per_slot_1h);
    m.vote_credits_per_slot_epoch.set(credits_per_slot_epoch);
    m.vote_latency_slots_5m.set(latency_5m);
    m.vote_latency_slots_1h.set(latency_1h);
    m.vote_latency_slots_epoch.set(latency_epoch);
    m.expected_max.set(expected_max_rooted as i64);
    m.actual.set(cur.credits_this_epoch as i64);
    m.missed_current_epoch.set(missed_now as i64);
    m.projected_credits_epoch.set(projected_credits);

    append_log(state.prev.as_ref().map(|(s, t)| (s, *t)), &cur, now);
    state.prev = Some((cur, now));

    Ok(())
}

pub async fn run_poll<C: RpcClient + Sync>(
    rpc: &C,
    args: &Args,
    m: std::sync::Arc<Metrics>,
) -> Result<()> {
    let mut state = PollState::new(args.interval_secs);

    loop {
        match poll_once(rpc, args, &m, &mut state).await {
            Ok(()) => {
                state.on_success();
                sleep(state.base_interval).await;
            } // OK
            Err(e) => {
                m.ws_connected.set(0);
                m.ws_errors.inc();
                tracing::warn!("poll failed: {e:#}");

                state.on_error();
                sleep(state.backoff).await;
            }
        } // match
    } //loop
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
    })
}

/// Statistics for a time window
struct WindowStats {
    missed: u64,
    elapsed: Duration,
    #[allow(dead_code)] // Used in tests
    slots: u64,
    expected_max: u64,
}

/// Calculate window statistics including missed credits, elapsed time, slots, and expected max
fn window_stats(
    hist: &VecDeque<HistEntry>,
    now: Instant,
    window: Duration,
    current_missed: u64,
    current_slot: u64,
) -> WindowStats {
    if hist.is_empty() {
        return WindowStats {
            missed: 0,
            elapsed: Duration::ZERO,
            slots: 0,
            expected_max: 0,
        };
    }

    let start = now - window;

    // Find the last entry strictly before the window start.
    // This ensures we capture ALL credits missed from the window start onwards.
    // If all entries are at or after start (running less than window duration), use the oldest.
    let (base_time, base_missed, base_slot) = hist
        .iter()
        .rev()
        .find(|(t, _, _)| *t < start)
        .or_else(|| hist.front())
        .map(|(t, m, s)| (*t, *m, *s))
        .unwrap_or((now, current_missed, current_slot));

    let missed = current_missed.saturating_sub(base_missed);
    let elapsed = now.saturating_duration_since(base_time);
    let slots = current_slot.saturating_sub(base_slot);
    let expected_max = slots.saturating_mul(MAX_CREDITS_PER_SLOT);

    WindowStats {
        missed,
        elapsed,
        slots,
        expected_max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to get just the missed delta
    fn window_delta(
        hist: &VecDeque<HistEntry>,
        now: Instant,
        window: Duration,
        current_missed: u64,
        current_slot: u64,
    ) -> u64 {
        window_stats(hist, now, window, current_missed, current_slot).missed
    }

    #[test]
    fn test_window_delta_empty_history() {
        let hist: VecDeque<HistEntry> = VecDeque::new();
        let now = Instant::now();
        let result = window_delta(&hist, now, Duration::from_secs(300), 100, 1000);
        assert_eq!(result, 0, "empty history should return 0");
    }

    #[test]
    fn test_window_delta_single_entry() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();
        hist.push_back((now, 100, 1000));

        // Window covers more than our data - should use oldest entry as base
        let result = window_delta(&hist, now, Duration::from_secs(300), 100, 1000);
        assert_eq!(
            result, 0,
            "single entry at current time should give 0 delta"
        );

        // With higher current value
        let result = window_delta(&hist, now, Duration::from_secs(300), 150, 1000);
        assert_eq!(result, 50, "delta should be current - oldest entry");
    }

    #[test]
    fn test_window_delta_uses_last_entry_before_window() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // Entries at: t-600s (10min ago), t-400s, t-200s, t-0s
        hist.push_back((now - Duration::from_secs(600), 100, 1000)); // before 5m window
        hist.push_back((now - Duration::from_secs(400), 200, 1100)); // before 5m window
        hist.push_back((now - Duration::from_secs(200), 300, 1200)); // within 5m
        hist.push_back((now, 400, 1300)); // current

        // 5-minute window: should use entry at t-400s (last before t-300s)
        let result = window_delta(&hist, now, Duration::from_secs(300), 400, 1300);
        // base = 200 (entry at t-400s, which is last entry <= t-300s)
        // delta = 400 - 200 = 200
        assert_eq!(result, 200, "should use last entry before window start");
    }

    #[test]
    fn test_window_delta_all_within_window() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // All entries within 5 minutes (running for 3 minutes)
        hist.push_back((now - Duration::from_secs(180), 0, 1000)); // 3 min ago
        hist.push_back((now - Duration::from_secs(120), 50, 1050)); // 2 min ago
        hist.push_back((now - Duration::from_secs(60), 100, 1100)); // 1 min ago
        hist.push_back((now, 150, 1150)); // now

        // 5-minute window: all entries are after start, use oldest (first) entry
        let result = window_delta(&hist, now, Duration::from_secs(300), 150, 1150);
        // base = 0 (oldest entry)
        // delta = 150 - 0 = 150
        assert_eq!(
            result, 150,
            "should use oldest entry when all are within window"
        );
    }

    #[test]
    fn test_window_delta_1h_vs_5m() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // 15 minutes of data
        hist.push_back((now - Duration::from_secs(900), 0, 1000)); // 15 min ago (start)
        hist.push_back((now - Duration::from_secs(600), 3000, 2000)); // 10 min ago
        hist.push_back((now - Duration::from_secs(300), 6000, 3000)); // 5 min ago
        hist.push_back((now, 9000, 4000)); // now

        let current = 9000u64;
        let current_slot = 4000u64;

        // 5-minute window: base from entry at t-600s (last <= t-300s)
        let missed_5m = window_delta(&hist, now, Duration::from_secs(300), current, current_slot);
        // base = 3000, delta = 9000 - 3000 = 6000
        assert_eq!(
            missed_5m, 6000,
            "5m window should capture credits from t-600 to now"
        );

        // 1-hour window: all entries within, use oldest
        let missed_1h = window_delta(&hist, now, Duration::from_secs(3600), current, current_slot);
        // base = 0, delta = 9000
        assert_eq!(missed_1h, 9000, "1h window should capture all credits");

        // 1h should be >= 5m
        assert!(
            missed_1h >= missed_5m,
            "1h window should capture at least as much as 5m: 1h={} 5m={}",
            missed_1h,
            missed_5m
        );
    }

    #[test]
    fn test_window_delta_exact_boundary() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // Entry exactly at window boundary - should NOT be used as base (we use < start, not <=)
        hist.push_back((now - Duration::from_secs(300), 100, 1000)); // exactly 5 min ago
        hist.push_back((now, 200, 1100));

        let result = window_delta(&hist, now, Duration::from_secs(300), 200, 1100);
        // Entry at t-300s is exactly at start (now - 300s), we need entry BEFORE start
        // No entry before t-300s, so we use oldest (which is t-300s)
        // base = 100, delta = 200 - 100 = 100
        assert_eq!(result, 100, "when no entry before boundary, use oldest");
    }

    #[test]
    fn test_window_delta_entry_before_boundary() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // Entry before window boundary
        hist.push_back((now - Duration::from_secs(400), 50, 1000)); // before 5 min window
        hist.push_back((now - Duration::from_secs(300), 100, 1050)); // exactly at boundary
        hist.push_back((now, 200, 1150));

        let result = window_delta(&hist, now, Duration::from_secs(300), 200, 1150);
        // Entry at t-400s is before start (t-300s), should be used as base
        // base = 50, delta = 200 - 50 = 150
        assert_eq!(result, 150, "should use last entry before window start");
    }

    #[test]
    fn test_efficiency_calculation() {
        let now = Instant::now();
        let mut hist: VecDeque<HistEntry> = VecDeque::new();

        // Simulate 100 slots with 800 missed credits (out of 1600 max = 100 * 16)
        hist.push_back((now - Duration::from_secs(60), 0, 1000));
        hist.push_back((now, 800, 1100)); // 100 slots elapsed, 800 missed

        let stats = window_stats(&hist, now, Duration::from_secs(300), 800, 1100);
        assert_eq!(stats.slots, 100, "should have 100 slots");
        assert_eq!(stats.expected_max, 1600, "expected max = 100 * 16");
        assert_eq!(stats.missed, 800, "missed should be 800");

        // efficiency = 1 - (800/1600) = 0.5
        let efficiency = 1.0 - (stats.missed as f64 / stats.expected_max as f64);
        assert!(
            (efficiency - 0.5).abs() < 0.001,
            "efficiency should be 0.5, got {}",
            efficiency
        );
    }

    #[test]
    fn test_poll_state_epoch_tracking() {
        let mut state = PollState::new(60);
        assert!(state.prev_epoch.is_none(), "initial epoch should be None");

        // Simulate first poll
        state.prev_epoch = Some(100);
        state.prev_missed = Some(5000);

        // Check epoch change detection
        let epoch_changed = state.prev_epoch.map(|e| e != 101).unwrap_or(false);
        assert!(epoch_changed, "should detect epoch change from 100 to 101");

        let same_epoch = state.prev_epoch.map(|e| e != 100).unwrap_or(false);
        assert!(!same_epoch, "should not detect change when same epoch");
    }
}
