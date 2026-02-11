use std::collections::{HashSet, VecDeque};
use std::time::Instant;

/// Histogram entry: (timestamp, credits_bucket_counts, missed_credits_cumulative)
/// credits_bucket_counts[i] = count of votes that earned i credits (0..=16)
type HistEntry = (Instant, [u64; 17], u64);

/// Tracks per-vote TVC credits and builds histograms
/// Uses epoch_credits as source of truth for missed credits accounting
#[derive(Debug)]
pub struct VoteTracker {
    /// Previously seen vote slots (to detect new votes)
    prev_votes: HashSet<u64>,
    /// Previous root slot (for missed slot detection)
    prev_root_slot: Option<u64>,
    /// Previous epoch credits value (for delta calculation)
    prev_epoch_credits: Option<u64>,
    /// Histogram of credits earned this epoch: counts[i] = votes earning i credits
    epoch_histogram: [u64; 17],
    /// Current epoch (to detect epoch changes)
    current_epoch: Option<u64>,
    /// Rolling history for time-windowed histograms
    hist: VecDeque<HistEntry>,
    /// Cumulative histogram (for computing deltas)
    cumulative_histogram: [u64; 17],
    /// Cumulative missed credits (for window calculations)
    cumulative_missed: u64,
    /// Missed credits this epoch
    epoch_missed: u64,
}

impl VoteTracker {
    pub fn new() -> Self {
        Self {
            prev_votes: HashSet::new(),
            prev_root_slot: None,
            prev_epoch_credits: None,
            epoch_histogram: [0; 17],
            current_epoch: None,
            hist: VecDeque::new(),
            cumulative_histogram: [0; 17],
            cumulative_missed: 0,
            epoch_missed: 0,
        }
    }

    /// Process a vote account update and return histogram updates
    ///
    /// Uses epoch_credits as source of truth for missed credits calculation.
    /// The histogram tracks per-vote credits, and missed slots are inferred
    /// from the difference between expected and actual credits.
    ///
    /// `votes` is a slice of (slot, confirmation_count, latency) tuples
    /// `epoch_credits` is the current cumulative epoch credits from the vote account
    pub fn process_update(
        &mut self,
        context_slot: u64,
        votes: &[(u64, u32, Option<u32>)], // (slot, confirmation_count, latency)
        root_slot: Option<u64>,
        epoch: u64,
        epoch_credits: u64, // Current cumulative epoch credits
    ) -> UpdateResult {
        let now = Instant::now();

        // Detect epoch change - reset epoch histogram and missed counter
        let epoch_changed = self.current_epoch.is_some_and(|e| e != epoch);
        if epoch_changed {
            self.epoch_histogram = [0; 17];
            self.epoch_missed = 0;
            self.prev_epoch_credits = None;
            self.prev_root_slot = None;
        }
        self.current_epoch = Some(epoch);

        // Get current vote slots
        let current_votes: HashSet<u64> = votes.iter().map(|(slot, _, _)| *slot).collect();

        // Build a map of slot -> latency for new vote credit calculation
        let vote_latencies: std::collections::HashMap<u64, Option<u32>> = votes
            .iter()
            .map(|(slot, _, latency)| (*slot, *latency))
            .collect();

        // Find new votes (in current but not in previous)
        let new_votes: Vec<u64> = current_votes
            .difference(&self.prev_votes)
            .copied()
            .collect();

        // Calculate credits for each new vote
        let mut update_histogram = [0u64; 17];

        for vote_slot in &new_votes {
            let credits = if let Some(Some(latency)) = vote_latencies.get(vote_slot) {
                // Use the latency field from the vote account
                // latency = 1 means 16 credits, latency = 17 means 0 credits
                17u64.saturating_sub(*latency as u64).min(16)
            } else {
                // Fall back to inferring from context slot
                let gap = context_slot.saturating_sub(*vote_slot);
                16u64.saturating_sub(gap).min(16)
            };

            update_histogram[credits as usize] += 1;
            self.epoch_histogram[credits as usize] += 1;
            self.cumulative_histogram[credits as usize] += 1;
        }

        // Calculate missed credits using epoch_credits as source of truth
        // This accounts for BOTH late votes AND missed slots
        let mut missed_this_update = 0u64;
        if let (Some(prev_root), Some(curr_root), Some(prev_credits)) =
            (self.prev_root_slot, root_slot, self.prev_epoch_credits)
        {
            if curr_root > prev_root {
                let slots_rooted = curr_root - prev_root;
                let expected_credits = slots_rooted * 16;
                let actual_credits_delta = epoch_credits.saturating_sub(prev_credits);
                missed_this_update = expected_credits.saturating_sub(actual_credits_delta);

                // Add missed credits to cumulative total
                self.cumulative_missed += missed_this_update;
                self.epoch_missed += missed_this_update;
            }
        }

        // Store history entry for windowed calculations
        self.hist
            .push_back((now, self.cumulative_histogram, self.cumulative_missed));

        // Prune history older than 1 hour
        let cutoff = now - std::time::Duration::from_secs(3600);
        while let Some((t, _, _)) = self.hist.front() {
            if *t < cutoff {
                self.hist.pop_front();
            } else {
                break;
            }
        }

        // Update state
        self.prev_votes = current_votes;
        self.prev_root_slot = root_slot;
        self.prev_epoch_credits = Some(epoch_credits);

        UpdateResult {
            new_votes: new_votes.len() as u64,
            missed_credits: missed_this_update,
            update_histogram,
        }
    }

    /// Get histogram for a time window
    pub fn window_histogram(&self, window_secs: u64) -> [u64; 17] {
        if self.hist.is_empty() {
            return [0; 17];
        }

        let now = Instant::now();
        let start = now - std::time::Duration::from_secs(window_secs);

        // Find the last entry BEFORE the window start
        // If no entry exists before the window, use zeros as baseline
        let base = self
            .hist
            .iter()
            .rev()
            .find(|(t, _, _)| *t < start)
            .map(|(_, h, _)| *h)
            .unwrap_or([0; 17]);

        // Calculate delta from baseline to current
        let mut result = [0u64; 17];
        for i in 0..17 {
            result[i] = self.cumulative_histogram[i].saturating_sub(base[i]);
        }
        result
    }

    /// Get missed credits for a time window
    pub fn window_missed(&self, window_secs: u64) -> u64 {
        if self.hist.is_empty() {
            return 0;
        }

        let now = Instant::now();
        let start = now - std::time::Duration::from_secs(window_secs);

        // Find the last entry BEFORE the window start
        let base = self
            .hist
            .iter()
            .rev()
            .find(|(t, _, _)| *t < start)
            .map(|(_, _, m)| *m)
            .unwrap_or(0);

        self.cumulative_missed.saturating_sub(base)
    }

    /// Get epoch histogram
    pub fn epoch_histogram(&self) -> [u64; 17] {
        self.epoch_histogram
    }

    /// Get epoch missed credits
    pub fn epoch_missed(&self) -> u64 {
        self.epoch_missed
    }

    /// Calculate total votes in histogram
    pub fn histogram_total(hist: &[u64; 17]) -> u64 {
        hist.iter().sum()
    }

    /// Calculate total credits earned from histogram
    pub fn histogram_credits(hist: &[u64; 17]) -> u64 {
        hist.iter()
            .enumerate()
            .map(|(credits, count)| credits as u64 * count)
            .sum()
    }

    /// Calculate histogram as fractions (0.0 to 1.0)
    pub fn histogram_fractions(hist: &[u64; 17]) -> [f64; 17] {
        let total = Self::histogram_total(hist);
        if total == 0 {
            return [0.0; 17];
        }
        let mut result = [0.0; 17];
        for i in 0..17 {
            result[i] = hist[i] as f64 / total as f64;
        }
        result
    }

    /// Calculate efficiency from histogram (actual credits / max possible)
    /// This is a more accurate per-vote efficiency than epoch-level totals
    pub fn histogram_efficiency(hist: &[u64; 17]) -> f64 {
        let total_votes = Self::histogram_total(hist);
        if total_votes == 0 {
            return 0.0;
        }
        let max_credits = total_votes * 16; // Max 16 credits per vote
        let actual_credits: u64 = hist
            .iter()
            .enumerate()
            .map(|(credits, count)| credits as u64 * count)
            .sum();
        actual_credits as f64 / max_credits as f64
    }

    /// Calculate average credits per vote from histogram
    pub fn histogram_avg_credits(hist: &[u64; 17]) -> f64 {
        let total_votes = Self::histogram_total(hist);
        if total_votes == 0 {
            return 0.0;
        }
        let total_credits: u64 = hist
            .iter()
            .enumerate()
            .map(|(credits, count)| credits as u64 * count)
            .sum();
        total_credits as f64 / total_votes as f64
    }
}

impl Default for VoteTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub new_votes: u64,
    pub missed_credits: u64,
    pub update_histogram: [u64; 17],
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: simulate epoch_credits as cumulative based on votes
    // For testing, we just increment by 16 per slot (assuming perfect voting)
    fn sim_epoch_credits(root_slot: u64) -> u64 {
        root_slot * 16
    }

    #[test]
    fn test_vote_tracker_new() {
        let tracker = VoteTracker::new();
        assert!(tracker.prev_votes.is_empty());
        assert_eq!(tracker.epoch_histogram, [0; 17]);
    }

    #[test]
    fn test_credits_calculation_with_latency() {
        let mut tracker = VoteTracker::new();

        // First update establishes baseline - latency=1 means 16 credits
        let votes = vec![(1000, 1, Some(1))];
        let result = tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // latency=1 → 16 credits

        // Add more votes with different latencies
        let votes = vec![
            (1000, 2, Some(1)),
            (1001, 1, Some(2)), // latency=2 → 15 credits
        ];
        let result = tracker.process_update(1001, &votes, Some(1000), 100, sim_epoch_credits(1000));
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[15], 1); // 15 credits

        // Test with latency=3 → 14 credits
        let votes = vec![(1000, 3, Some(1)), (1001, 2, Some(2)), (1002, 1, Some(3))];
        let result = tracker.process_update(1002, &votes, Some(1001), 100, sim_epoch_credits(1001));
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // latency=3 → 14 credits
    }

    #[test]
    fn test_credits_calculation_fallback() {
        let mut tracker = VoteTracker::new();

        // Test fallback when latency is None (use context_slot - vote_slot)
        let votes = vec![(1000, 1, None)];
        let result = tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // gap=0 → 16 credits

        // Vote for slot 1001 seen at context slot 1003 (gap=2)
        let votes = vec![(1000, 2, None), (1001, 1, None)];
        let result = tracker.process_update(1003, &votes, Some(1000), 100, sim_epoch_credits(1000));
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // gap=2 → 14 credits
    }

    #[test]
    fn test_epoch_reset() {
        let mut tracker = VoteTracker::new();

        // Build up some histogram in epoch 100
        let votes = vec![(1000, 1, Some(1))];
        tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));

        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(1000), 100, sim_epoch_credits(1000));

        let count_before = tracker.epoch_histogram.iter().sum::<u64>();
        assert!(count_before > 0, "should have votes in epoch 100");

        // New epoch should reset
        let votes = vec![(2000, 1, Some(1))];
        tracker.process_update(2000, &votes, Some(1999), 101, 16); // New epoch, reset credits

        // Epoch histogram should only contain the new vote
        assert_eq!(
            tracker.epoch_histogram.iter().sum::<u64>(),
            1,
            "epoch histogram should reset on new epoch"
        );
        assert_eq!(tracker.epoch_missed, 0, "epoch missed should reset");
    }

    #[test]
    fn test_histogram_fractions() {
        let hist = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 20, 70];
        let fracs = VoteTracker::histogram_fractions(&hist);
        assert!((fracs[14] - 0.1).abs() < 0.001);
        assert!((fracs[15] - 0.2).abs() < 0.001);
        assert!((fracs[16] - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_histogram_fractions_empty() {
        let hist = [0; 17];
        let fracs = VoteTracker::histogram_fractions(&hist);
        assert!(fracs.iter().all(|&f| f == 0.0));
    }

    #[test]
    fn test_histogram_total() {
        let hist = [1, 2, 3, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(VoteTracker::histogram_total(&hist), 15);
    }

    #[test]
    fn test_histogram_credits() {
        // 1 vote at 16 credits, 2 at 15, 3 at 14 = 16 + 30 + 42 = 88
        let hist = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 1];
        assert_eq!(VoteTracker::histogram_credits(&hist), 16 + 30 + 42);
    }

    #[test]
    fn test_window_histogram() {
        let mut tracker = VoteTracker::new();

        // First update
        let votes = vec![(1000, 1, Some(1))]; // 16 credits
        tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));

        // Second update
        let votes = vec![
            (1000, 2, Some(1)),
            (1001, 1, Some(2)), // 15 credits (new)
        ];
        tracker.process_update(1001, &votes, Some(1000), 100, sim_epoch_credits(1000));

        // Third update
        let votes = vec![
            (1000, 3, Some(1)),
            (1001, 2, Some(2)),
            (1002, 1, Some(1)), // 16 credits (new)
        ];
        tracker.process_update(1002, &votes, Some(1001), 100, sim_epoch_credits(1001));

        // Check cumulative histogram
        assert_eq!(tracker.cumulative_histogram[16], 2); // Two votes at 16 credits
        assert_eq!(tracker.cumulative_histogram[15], 1); // One vote at 15 credits

        let hist = tracker.window_histogram(300); // 5 minute window
        assert_eq!(hist[16], 2);
        assert_eq!(hist[15], 1);

        assert_eq!(hist, tracker.epoch_histogram());
    }

    #[test]
    fn test_latency_to_credits_edge_cases() {
        let mut tracker = VoteTracker::new();

        // Test latency = 17 (0 credits - very slow)
        let votes = vec![(1000, 1, Some(17))];
        let result = tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));
        assert_eq!(result.update_histogram[0], 1);

        // Test latency = 1 (16 credits - fastest)
        let votes = vec![(1000, 2, Some(17)), (1001, 1, Some(1))];
        let result = tracker.process_update(1001, &votes, Some(1000), 100, sim_epoch_credits(1000));
        assert_eq!(result.update_histogram[16], 1);

        // Test latency > 17 (should still be 0 credits)
        tracker.prev_votes.clear();
        let votes = vec![(2000, 1, Some(100))];
        let result = tracker.process_update(2000, &votes, Some(1999), 100, sim_epoch_credits(1999));
        assert_eq!(result.update_histogram[0], 1);
    }

    #[test]
    fn test_multiple_updates_accumulate() {
        let mut tracker = VoteTracker::new();

        // First batch
        let votes = vec![(1000, 1, Some(1))];
        tracker.process_update(1000, &votes, Some(999), 100, sim_epoch_credits(999));
        assert_eq!(tracker.cumulative_histogram[16], 1);

        // Second batch
        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(1000), 100, sim_epoch_credits(1000));
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Third batch
        let votes = vec![(1000, 3, Some(1)), (1001, 2, Some(1)), (1002, 1, Some(1))];
        tracker.process_update(1002, &votes, Some(1001), 100, sim_epoch_credits(1001));
        assert_eq!(tracker.cumulative_histogram[16], 3);

        assert_eq!(
            VoteTracker::histogram_total(&tracker.cumulative_histogram),
            3
        );
    }

    #[test]
    fn test_duplicate_votes_not_counted() {
        let mut tracker = VoteTracker::new();

        // First update
        let votes = vec![(1000, 1, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(999), 100, sim_epoch_credits(999));
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Same votes again (should not be counted again)
        let votes = vec![(1000, 2, Some(1)), (1001, 2, Some(1))];
        let result = tracker.process_update(1002, &votes, Some(1000), 100, sim_epoch_credits(1000));
        assert_eq!(result.new_votes, 0);
        assert_eq!(tracker.cumulative_histogram[16], 2); // Still 2
    }

    #[test]
    fn test_fresh_tracker_1h_equals_epoch() {
        let mut tracker = VoteTracker::new();

        let votes: Vec<(u64, u32, Option<u32>)> = vec![
            (1000, 1, Some(1)),  // 16 credits
            (1001, 1, Some(2)),  // 15 credits
            (1002, 1, Some(3)),  // 14 credits
            (1003, 1, Some(16)), // 1 credit
            (1004, 1, Some(17)), // 0 credits
        ];
        tracker.process_update(1004, &votes, Some(999), 100, sim_epoch_credits(999));

        let hist_1h = tracker.window_histogram(3600);
        let hist_epoch = tracker.epoch_histogram();

        assert_eq!(
            hist_1h, hist_epoch,
            "1h and epoch should match on fresh tracker"
        );
        assert_eq!(
            tracker.window_histogram(300),
            hist_epoch,
            "5m and epoch should match"
        );

        assert_eq!(hist_epoch[16], 1);
        assert_eq!(hist_epoch[15], 1);
        assert_eq!(hist_epoch[14], 1);
        assert_eq!(hist_epoch[1], 1);
        assert_eq!(hist_epoch[0], 1);
    }

    #[test]
    fn test_missed_credits_tracking() {
        let mut tracker = VoteTracker::new();

        // First update: establishes baseline, no missed credits yet
        let votes = vec![(1000, 1, Some(1))];
        let result = tracker.process_update(1000, &votes, Some(999), 100, 16); // 1 slot = 16 credits
        assert_eq!(result.missed_credits, 0); // First update, no previous to compare

        // Second update: root advances by 1, earned 16 credits, no missed
        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        let result = tracker.process_update(1001, &votes, Some(1000), 100, 32); // 2 slots = 32 credits
        assert_eq!(result.missed_credits, 0); // Perfect voting

        // Third update: root advances by 2 slots, but only earned 16 credits (missed 16)
        let votes = vec![(1000, 3, Some(1)), (1001, 2, Some(1)), (1002, 1, Some(1))];
        let result = tracker.process_update(1002, &votes, Some(1002), 100, 48); // 3 slots = 48 credits, but only 16 delta
        // Expected: (1002 - 1000) * 16 = 32 credits expected, but delta = 48 - 32 = 16 earned
        // Missed = 32 - 16 = 16
        assert_eq!(result.missed_credits, 16);

        // Verify cumulative missed
        assert_eq!(tracker.cumulative_missed, 16);
        assert_eq!(tracker.epoch_missed, 16);
    }

    #[test]
    fn test_window_missed() {
        let mut tracker = VoteTracker::new();

        // Establish baseline
        let votes = vec![(1000, 1, Some(1))];
        tracker.process_update(1000, &votes, Some(999), 100, 16);

        // Add some missed credits
        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(1001), 100, 24); // 2 slots expected 32, got 24-16=8, missed 24
        // Wait, let me recalculate:
        // prev_root = 999, curr_root = 1001
        // slots_rooted = 1001 - 999 = 2
        // expected = 2 * 16 = 32
        // prev_credits = 16, curr_credits = 24
        // delta = 24 - 16 = 8
        // missed = 32 - 8 = 24

        let missed_5m = tracker.window_missed(300);
        let missed_epoch = tracker.epoch_missed();

        // For a fresh tracker, 5m missed should equal epoch missed
        assert_eq!(missed_5m, missed_epoch);
    }
}
