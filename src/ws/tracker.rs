use std::collections::{HashSet, VecDeque};
use std::time::Instant;

/// Histogram entry: (timestamp, credits_bucket_counts)
/// credits_bucket_counts[i] = count of votes that earned i credits (0..=16)
type HistEntry = (Instant, [u64; 17]);

/// Histogram entry with slot tracking: (timestamp, credits_bucket_counts, slots_elapsed)
type SlotHistEntry = (Instant, [u64; 17], u64);

/// Tracks per-vote TVC credits and builds histograms
#[derive(Debug)]
pub struct VoteTracker {
    /// Previously seen vote slots (to detect new votes)
    prev_votes: HashSet<u64>,
    /// Previous context slot (for latency calculation)
    prev_context_slot: Option<u64>,
    /// Previous root slot
    prev_root_slot: Option<u64>,
    /// Histogram of credits earned this epoch: counts[i] = votes earning i credits
    epoch_histogram: [u64; 17],
    /// Current epoch (to detect epoch changes)
    current_epoch: Option<u64>,
    /// Rolling history for time-windowed histograms
    hist: VecDeque<HistEntry>,
    /// Cumulative histogram (for computing deltas)
    cumulative_histogram: [u64; 17],
    /// First root slot seen (for elapsed slot calculation)
    first_root_slot: Option<u64>,
    /// Cumulative slots elapsed since first update
    cumulative_slots: u64,
    /// Rolling history with slot tracking for missed credits calculation
    slot_hist: VecDeque<SlotHistEntry>,
}

impl VoteTracker {
    pub fn new() -> Self {
        Self {
            prev_votes: HashSet::new(),
            prev_context_slot: None,
            prev_root_slot: None,
            epoch_histogram: [0; 17],
            current_epoch: None,
            hist: VecDeque::new(),
            cumulative_histogram: [0; 17],
            first_root_slot: None,
            cumulative_slots: 0,
            slot_hist: VecDeque::new(),
        }
    }

    /// Process a vote account update and return histogram updates
    /// Returns (new_votes_histogram, missed_count) for this update
    ///
    /// `votes` is a slice of (slot, confirmation_count, latency) tuples
    /// where latency is the vote latency in slots (1 = fastest, earns 16 credits)
    pub fn process_update(
        &mut self,
        context_slot: u64,
        votes: &[(u64, u32, Option<u32>)], // (slot, confirmation_count, latency)
        root_slot: Option<u64>,
        epoch: u64,
    ) -> UpdateResult {
        let now = Instant::now();

        // Detect epoch change - reset epoch histogram
        let epoch_changed = self.current_epoch.map(|e| e != epoch).unwrap_or(false);
        if epoch_changed {
            self.epoch_histogram = [0; 17];
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
        // If latency is available from the RPC, use it directly: credits = 17 - latency
        // Otherwise, fall back to: credits = 16 - (context_slot - vote_slot)
        let mut update_histogram = [0u64; 17];
        let mut missed_count = 0u64;

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

            if credits == 0 {
                missed_count += 1;
            }
        }

        // Track elapsed slots for missed credits calculation
        // Using root_slot as the authoritative "finalized slot count"
        if let Some(root) = root_slot {
            if self.first_root_slot.is_none() {
                self.first_root_slot = Some(root);
            }
            // Calculate new slots since last update
            if let Some(prev_root) = self.prev_root_slot {
                let new_slots = root.saturating_sub(prev_root);
                self.cumulative_slots += new_slots;
            }
        }

        // Store history entry for windowed calculations
        self.hist.push_back((now, self.cumulative_histogram));
        self.slot_hist
            .push_back((now, self.cumulative_histogram, self.cumulative_slots));

        // Prune history older than 1 hour
        let cutoff = now - std::time::Duration::from_secs(3600);
        while let Some((t, _, _)) = self.slot_hist.front() {
            if *t < cutoff {
                self.slot_hist.pop_front();
            } else {
                break;
            }
        }
        while let Some((t, _)) = self.hist.front() {
            if *t < cutoff {
                self.hist.pop_front();
            } else {
                break;
            }
        }

        // Update state
        self.prev_votes = current_votes;
        self.prev_context_slot = Some(context_slot);
        self.prev_root_slot = root_slot;

        UpdateResult {
            new_votes: new_votes.len() as u64,
            missed_count,
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
        // (meaning all our data is within the window, so return everything)
        let base = self
            .hist
            .iter()
            .rev()
            .find(|(t, _)| *t < start)
            .map(|(_, h)| *h)
            .unwrap_or([0; 17]); // Use zeros if all data is within window

        // Calculate delta from baseline to current
        let mut result = [0u64; 17];
        for i in 0..17 {
            result[i] = self.cumulative_histogram[i].saturating_sub(base[i]);
        }
        result
    }

    /// Get epoch histogram
    pub fn epoch_histogram(&self) -> [u64; 17] {
        self.epoch_histogram
    }

    /// Calculate total votes in histogram
    pub fn histogram_total(hist: &[u64; 17]) -> u64 {
        hist.iter().sum()
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

    /// Calculate total credits earned from histogram
    pub fn histogram_credits(hist: &[u64; 17]) -> u64 {
        hist.iter()
            .enumerate()
            .map(|(credits, count)| credits as u64 * count)
            .sum()
    }

    /// Get slots elapsed and histogram for a time window
    /// Returns (histogram, slots_elapsed, actual_credits)
    pub fn window_stats(&self, window_secs: u64) -> (u64, u64, u64) {
        if self.slot_hist.is_empty() {
            return (0, 0, 0);
        }

        let now = Instant::now();
        let start = now - std::time::Duration::from_secs(window_secs);

        // Find baseline entry (last entry before window start, or zeros if all within window)
        let base = self.slot_hist.iter().rev().find(|(t, _, _)| *t < start);

        let (base_hist, base_slots) = match base {
            Some((_, h, s)) => (*h, *s),
            None => ([0u64; 17], 0u64),
        };

        // Calculate deltas
        let hist_delta: [u64; 17] = {
            let mut result = [0u64; 17];
            for i in 0..17 {
                result[i] = self.cumulative_histogram[i].saturating_sub(base_hist[i]);
            }
            result
        };

        let slots_delta = self.cumulative_slots.saturating_sub(base_slots);
        let credits_earned = Self::histogram_credits(&hist_delta);

        (
            slots_delta,
            Self::histogram_total(&hist_delta),
            credits_earned,
        )
    }

    /// Calculate missed credits for a time window
    /// missed = expected_max (slots * 16) - actual_credits
    pub fn window_missed_credits(&self, window_secs: u64) -> u64 {
        let (slots, _votes, credits) = self.window_stats(window_secs);
        let expected = slots * 16;
        expected.saturating_sub(credits)
    }

    /// Get total cumulative slots since tracker started
    pub fn total_slots(&self) -> u64 {
        self.cumulative_slots
    }

    /// Get total cumulative credits since tracker started
    pub fn total_credits(&self) -> u64 {
        Self::histogram_credits(&self.cumulative_histogram)
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
    pub missed_count: u64,
    pub update_histogram: [u64; 17],
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = tracker.process_update(1000, &votes, Some(999), 100);
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // latency=1 → 16 credits

        // Add more votes with different latencies
        let votes = vec![
            (1000, 2, Some(1)),
            (1001, 1, Some(2)), // latency=2 → 15 credits
        ];
        let result = tracker.process_update(1001, &votes, Some(1000), 100);
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[15], 1); // 15 credits

        // Test with latency=3 → 14 credits
        let votes = vec![(1000, 3, Some(1)), (1001, 2, Some(2)), (1002, 1, Some(3))];
        let result = tracker.process_update(1002, &votes, Some(1001), 100);
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // latency=3 → 14 credits
    }

    #[test]
    fn test_credits_calculation_fallback() {
        let mut tracker = VoteTracker::new();

        // Test fallback when latency is None (use context_slot - vote_slot)
        let votes = vec![(1000, 1, None)];
        let result = tracker.process_update(1000, &votes, Some(999), 100);
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // gap=0 → 16 credits

        // Vote for slot 1001 seen at context slot 1003 (gap=2)
        let votes = vec![(1000, 2, None), (1001, 1, None)];
        let result = tracker.process_update(1003, &votes, Some(1000), 100);
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // gap=2 → 14 credits
    }

    #[test]
    fn test_epoch_reset() {
        let mut tracker = VoteTracker::new();

        // Build up some histogram in epoch 100
        let votes = vec![(1000, 1, Some(1))];
        tracker.process_update(1000, &votes, Some(999), 100);

        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(1000), 100);

        let count_before = tracker.epoch_histogram.iter().sum::<u64>();
        assert!(count_before > 0, "should have votes in epoch 100");

        // New epoch should reset - clear prev state to simulate epoch boundary
        tracker.prev_votes.clear();
        tracker.prev_root_slot = None;

        let votes = vec![(2000, 1, Some(1))];
        tracker.process_update(2000, &votes, Some(1999), 101);

        // Epoch histogram should only contain the new vote
        assert_eq!(
            tracker.epoch_histogram.iter().sum::<u64>(),
            1,
            "epoch histogram should reset on new epoch"
        );
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
    fn test_window_histogram() {
        let mut tracker = VoteTracker::new();

        // First update
        let votes = vec![(1000, 1, Some(1))]; // 16 credits
        tracker.process_update(1000, &votes, Some(999), 100);

        // Second update
        let votes = vec![
            (1000, 2, Some(1)),
            (1001, 1, Some(2)), // 15 credits (new)
        ];
        tracker.process_update(1001, &votes, Some(1000), 100);

        // Third update
        let votes = vec![
            (1000, 3, Some(1)),
            (1001, 2, Some(2)),
            (1002, 1, Some(1)), // 16 credits (new)
        ];
        tracker.process_update(1002, &votes, Some(1001), 100);

        // Check cumulative histogram
        assert_eq!(tracker.cumulative_histogram[16], 2); // Two votes at 16 credits
        assert_eq!(tracker.cumulative_histogram[15], 1); // One vote at 15 credits

        // When all data is within the window (tracker just started),
        // window_histogram should return the full cumulative histogram
        // (uses zeros as baseline since no entry exists before window start)
        let hist = tracker.window_histogram(300); // 5 minute window
        assert_eq!(hist[16], 2); // All votes at 16 credits
        assert_eq!(hist[15], 1); // All votes at 15 credits

        // Should match epoch histogram when running < 1 hour
        assert_eq!(hist, tracker.epoch_histogram());
    }

    #[test]
    fn test_latency_to_credits_edge_cases() {
        let mut tracker = VoteTracker::new();

        // Test latency = 17 (0 credits - very slow)
        let votes = vec![(1000, 1, Some(17))];
        let result = tracker.process_update(1000, &votes, Some(999), 100);
        assert_eq!(result.update_histogram[0], 1);

        // Test latency = 1 (16 credits - fastest)
        let votes = vec![(1000, 2, Some(17)), (1001, 1, Some(1))];
        let result = tracker.process_update(1001, &votes, Some(1000), 100);
        assert_eq!(result.update_histogram[16], 1);

        // Test latency > 17 (should still be 0 credits)
        tracker.prev_votes.clear();
        let votes = vec![(2000, 1, Some(100))];
        let result = tracker.process_update(2000, &votes, Some(1999), 100);
        assert_eq!(result.update_histogram[0], 1);
    }

    #[test]
    fn test_multiple_updates_accumulate() {
        let mut tracker = VoteTracker::new();

        // First batch
        let votes = vec![(1000, 1, Some(1))];
        tracker.process_update(1000, &votes, Some(999), 100);
        assert_eq!(tracker.cumulative_histogram[16], 1);

        // Second batch
        let votes = vec![(1000, 2, Some(1)), (1001, 1, Some(1))];
        tracker.process_update(1001, &votes, Some(1000), 100);
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Third batch
        let votes = vec![(1000, 3, Some(1)), (1001, 2, Some(1)), (1002, 1, Some(1))];
        tracker.process_update(1002, &votes, Some(1001), 100);
        assert_eq!(tracker.cumulative_histogram[16], 3);

        // Total should be 3
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
        tracker.process_update(1001, &votes, Some(999), 100);
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Same votes again (should not be counted again)
        let votes = vec![(1000, 2, Some(1)), (1001, 2, Some(1))];
        let result = tracker.process_update(1002, &votes, Some(1000), 100);
        assert_eq!(result.new_votes, 0);
        assert_eq!(tracker.cumulative_histogram[16], 2); // Still 2
    }

    #[test]
    fn test_fresh_tracker_1h_equals_epoch() {
        // When tracker is running < 1 hour, 1h and epoch windows should be identical
        let mut tracker = VoteTracker::new();

        // Add various votes with different credits
        let votes: Vec<(u64, u32, Option<u32>)> = vec![
            (1000, 1, Some(1)),  // 16 credits
            (1001, 1, Some(2)),  // 15 credits
            (1002, 1, Some(3)),  // 14 credits
            (1003, 1, Some(16)), // 1 credit
            (1004, 1, Some(17)), // 0 credits
        ];
        tracker.process_update(1004, &votes, Some(999), 100);

        let hist_1h = tracker.window_histogram(3600);
        let hist_epoch = tracker.epoch_histogram();

        // All windows should be identical for fresh tracker
        assert_eq!(
            hist_1h, hist_epoch,
            "1h and epoch should match on fresh tracker"
        );
        assert_eq!(
            tracker.window_histogram(300),
            hist_epoch,
            "5m and epoch should match"
        );

        // Verify actual content
        assert_eq!(hist_epoch[16], 1);
        assert_eq!(hist_epoch[15], 1);
        assert_eq!(hist_epoch[14], 1);
        assert_eq!(hist_epoch[1], 1);
        assert_eq!(hist_epoch[0], 1);
    }

    #[test]
    fn test_slot_tracking_and_missed_credits() {
        let mut tracker = VoteTracker::new();

        // First update: 2 votes, root_slot moves from None to 100
        let votes = vec![(99, 1, Some(1)), (100, 1, Some(1))]; // 2 votes at 16 credits
        tracker.process_update(100, &votes, Some(100), 1);
        assert_eq!(tracker.first_root_slot, Some(100));
        assert_eq!(tracker.cumulative_slots, 0); // First update, no delta yet

        // Second update: 2 more votes, root_slot moves to 110 (10 slots elapsed)
        let votes = vec![
            (99, 2, Some(1)),
            (100, 2, Some(1)),
            (109, 1, Some(2)), // 15 credits
            (110, 1, Some(1)), // 16 credits
        ];
        tracker.process_update(110, &votes, Some(110), 1);
        assert_eq!(tracker.cumulative_slots, 10); // 110 - 100 = 10 slots

        // Check total credits: 3 votes at 16 + 1 vote at 15 = 48 + 15 = 63
        assert_eq!(tracker.total_credits(), 63);

        // Expected for 10 slots: 10 * 16 = 160
        // Missed: 160 - 63 = 97
        // This accounts for: 4 votes made = 4 slots voted
        //                   10 - 4 = 6 slots missed = 6 * 16 = 96
        //                   + 1 credit missed from latency (15 instead of 16)
        //                   = 97 total missed
        let (slots, votes, credits) = tracker.window_stats(300);
        assert_eq!(slots, 10);
        assert_eq!(votes, 4);
        assert_eq!(credits, 63);

        let missed = tracker.window_missed_credits(300);
        assert_eq!(missed, 97);
    }

    #[test]
    fn test_histogram_efficiency_with_missed_slots() {
        let mut tracker = VoteTracker::new();

        // Simulate 10 slots elapsed but only 5 votes made (all at 16 credits)
        // This means 5 slots were missed entirely
        tracker.process_update(100, &[], Some(100), 1); // First update, establishes baseline
        tracker.prev_root_slot = Some(90); // Hack to make cumulative_slots = 10
        tracker.process_update(100, &[], Some(100), 1);

        // Now add 5 votes at 16 credits
        let votes = vec![
            (95, 1, Some(1)),
            (96, 1, Some(1)),
            (97, 1, Some(1)),
            (98, 1, Some(1)),
            (99, 1, Some(1)),
        ];
        tracker.process_update(100, &votes, Some(100), 1);

        // Per-vote efficiency: 5/5 = 100% (all votes at 16 credits)
        let hist = tracker.window_histogram(300);
        let per_vote_eff = VoteTracker::histogram_efficiency(&hist);
        assert!((per_vote_eff - 1.0).abs() < 0.001);

        // But true efficiency accounting for missed slots:
        // 10 slots * 16 = 160 expected
        // 5 votes * 16 = 80 actual
        // efficiency = 80/160 = 0.5
        let (slots, _votes, credits) = tracker.window_stats(300);
        if slots > 0 {
            let true_efficiency = credits as f64 / (slots * 16) as f64;
            assert!(true_efficiency < 1.0); // Should be < 100% due to missed slots
        }
    }
}
