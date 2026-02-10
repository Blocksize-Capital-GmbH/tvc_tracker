use std::collections::{HashSet, VecDeque};
use std::time::Instant;

/// Histogram entry: (timestamp, credits_bucket_counts)
/// credits_bucket_counts[i] = count of votes that earned i credits (0..=16)
type HistEntry = (Instant, [u64; 17]);

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

        // Note: We don't try to detect missed slots from gaps anymore
        // because the vote tower only contains successful votes.
        // Missed votes are already reflected in the aggregate metrics from REST polling.

        // Store history entry for windowed calculations
        self.hist.push_back((now, self.cumulative_histogram));

        // Prune history older than 1 hour
        let cutoff = now - std::time::Duration::from_secs(3600);
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

        // Find baseline (last entry before window start, or oldest)
        let base = self
            .hist
            .iter()
            .rev()
            .find(|(t, _)| *t < start)
            .or_else(|| self.hist.front())
            .map(|(_, h)| *h)
            .unwrap_or([0; 17]);

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

        // First update - establishes baseline in history
        let votes = vec![(1000, 1, Some(1))]; // 16 credits
        tracker.process_update(1000, &votes, Some(999), 100);

        // Second update - adds more votes
        let votes = vec![
            (1000, 2, Some(1)),
            (1001, 1, Some(2)), // 15 credits (new)
        ];
        tracker.process_update(1001, &votes, Some(1000), 100);

        // Third update - adds another vote
        let votes = vec![
            (1000, 3, Some(1)),
            (1001, 2, Some(2)),
            (1002, 1, Some(1)), // 16 credits (new)
        ];
        tracker.process_update(1002, &votes, Some(1001), 100);

        // Check cumulative histogram
        assert_eq!(tracker.cumulative_histogram[16], 2); // Two votes at 16 credits
        assert_eq!(tracker.cumulative_histogram[15], 1); // One vote at 15 credits

        // For window histogram, since all entries are within 5min window,
        // it uses the oldest entry as baseline. The delta from first entry
        // (which had 1 vote at 16) to now (2 votes at 16, 1 at 15) should be:
        // 16 credits: 2 - 1 = 1
        // 15 credits: 1 - 0 = 1
        let hist = tracker.window_histogram(300); // 5 minute window
        assert_eq!(hist[16], 1); // Delta since first entry
        assert_eq!(hist[15], 1);
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
}
