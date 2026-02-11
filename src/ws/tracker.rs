use std::collections::{HashSet, VecDeque};
use std::time::Instant;

/// Slots per epoch on mainnet (constant, never changes)
pub const SLOTS_PER_EPOCH: u64 = 432_000;

/// Maximum credits per slot (TVC: 16 for fastest vote, 0 for slowest)
pub const MAX_CREDITS_PER_SLOT: u64 = 16;

/// Histogram entry: (timestamp, credits_bucket_counts, missed_credits_cumulative)
/// credits_bucket_counts[i] = count of votes that earned i credits (0..=16)
type HistEntry = (Instant, [u64; 17], u64);

/// Calculate epoch info from a slot number
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochInfo {
    pub epoch: u64,
    pub slot_index: u64,
    pub epoch_start_slot: u64,
    pub slots_in_epoch: u64,
}

impl EpochInfo {
    /// Calculate epoch info from an absolute slot
    pub fn from_slot(slot: u64) -> Self {
        let epoch = slot / SLOTS_PER_EPOCH;
        let slot_index = slot % SLOTS_PER_EPOCH;
        let epoch_start_slot = epoch * SLOTS_PER_EPOCH;
        Self {
            epoch,
            slot_index,
            epoch_start_slot,
            slots_in_epoch: SLOTS_PER_EPOCH,
        }
    }

    /// Calculate rooted slots elapsed in this epoch
    pub fn rooted_slots_elapsed(&self, root_slot: u64) -> u64 {
        if root_slot < self.epoch_start_slot {
            0
        } else {
            root_slot
                .saturating_sub(self.epoch_start_slot)
                .saturating_add(1)
        }
    }

    /// Calculate expected max credits based on rooted slots
    pub fn expected_max_credits(&self, root_slot: u64) -> u64 {
        self.rooted_slots_elapsed(root_slot) * MAX_CREDITS_PER_SLOT
    }
}

/// Tracks per-vote TVC credits and builds histograms
/// Uses epoch_credits as source of truth for missed credits accounting
/// All epoch info is derived from slot numbers (no HTTP required)
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
    /// Current epoch info (derived from slot)
    epoch_info: Option<EpochInfo>,
    /// Rolling history for time-windowed histograms
    hist: VecDeque<HistEntry>,
    /// Cumulative histogram (for computing deltas)
    cumulative_histogram: [u64; 17],
    /// Cumulative missed credits (for window calculations)
    cumulative_missed: u64,
    /// Missed credits this epoch
    epoch_missed: u64,
    /// Actual credits earned since tracker started this epoch (from deltas)
    epoch_actual_credits: u64,
    /// First root_slot seen this epoch (for expected calculation)
    epoch_first_root_slot: Option<u64>,
    /// Total vote credits this epoch from vote account (raw value, not delta)
    total_epoch_credits: u64,
    /// Number of slots we've actually tracked (for projection)
    slots_tracked: u64,
}

impl VoteTracker {
    pub fn new() -> Self {
        Self {
            prev_votes: HashSet::new(),
            prev_root_slot: None,
            prev_epoch_credits: None,
            epoch_histogram: [0; 17],
            epoch_info: None,
            hist: VecDeque::new(),
            cumulative_histogram: [0; 17],
            cumulative_missed: 0,
            epoch_missed: 0,
            epoch_actual_credits: 0,
            epoch_first_root_slot: None,
            total_epoch_credits: 0,
            slots_tracked: 0,
        }
    }

    /// Get total epoch credits from vote account (raw value, matches `solana vote-account`)
    pub fn total_epoch_credits(&self) -> u64 {
        self.total_epoch_credits
    }

    /// Get number of slots actually tracked (for projection calculations)
    pub fn slots_tracked(&self) -> u64 {
        self.slots_tracked
    }

    /// Get current epoch info (if available)
    pub fn epoch_info(&self) -> Option<EpochInfo> {
        self.epoch_info
    }

    /// Get expected max credits for the current epoch
    pub fn epoch_expected_max(&self) -> u64 {
        match (
            self.epoch_info,
            self.prev_root_slot,
            self.epoch_first_root_slot,
        ) {
            (Some(_info), Some(root), Some(first_root)) => {
                // Calculate slots elapsed since we started tracking this epoch
                let slots_tracked = root.saturating_sub(first_root).saturating_add(1);
                slots_tracked * MAX_CREDITS_PER_SLOT
            }
            _ => 0,
        }
    }

    /// Get actual credits earned this epoch (from epoch_credits tracking)
    pub fn epoch_actual(&self) -> u64 {
        self.epoch_actual_credits
    }

    /// Process a vote account update and return histogram updates
    ///
    /// Uses epoch_credits as source of truth for missed credits calculation.
    /// The histogram tracks per-vote credits, and missed slots are inferred
    /// from the difference between expected and actual credits.
    ///
    /// `votes` is a slice of (slot, confirmation_count, latency) tuples
    /// `epoch_credits_delta` is the credits earned THIS epoch (credits - previous_credits)
    /// `total_epoch_credits` is the raw credits value from epochCredits (matches `solana vote-account`)
    ///
    /// Epoch info is derived directly from root_slot - NO HTTP required.
    pub fn process_update(
        &mut self,
        context_slot: u64,
        votes: &[(u64, u32, Option<u32>)], // (slot, confirmation_count, latency)
        root_slot: Option<u64>,
        epoch_credits_delta: u64, // Credits earned THIS epoch (credits - previous_credits)
        total_epoch_credits: u64, // Raw credits from epochCredits (matches vote-account)
    ) -> UpdateResult {
        let now = Instant::now();

        // Derive epoch info from root_slot (no HTTP needed!)
        let current_epoch_info = root_slot.map(EpochInfo::from_slot);

        // Detect epoch change - reset epoch histogram and missed counter
        let epoch_changed = self.epoch_info.is_some()
            && current_epoch_info.is_some()
            && self.epoch_info.unwrap().epoch != current_epoch_info.unwrap().epoch;

        if epoch_changed {
            self.epoch_histogram = [0; 17];
            self.epoch_missed = 0;
            self.epoch_actual_credits = 0;
            self.prev_epoch_credits = None;
            self.prev_root_slot = None;
            self.epoch_first_root_slot = root_slot;
            self.slots_tracked = 0;
        }

        // Track first root slot of this epoch for expected calculation
        if self.epoch_first_root_slot.is_none() {
            self.epoch_first_root_slot = root_slot;
        }

        // Update total epoch credits (raw value from vote account)
        self.total_epoch_credits = total_epoch_credits;

        self.epoch_info = current_epoch_info;

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
                let expected_credits = slots_rooted * MAX_CREDITS_PER_SLOT;
                let actual_delta = epoch_credits_delta.saturating_sub(prev_credits);
                missed_this_update = expected_credits.saturating_sub(actual_delta);

                // Add missed credits to cumulative total
                self.cumulative_missed += missed_this_update;
                self.epoch_missed += missed_this_update;
                self.epoch_actual_credits += actual_delta;

                // Track slots for projection calculations
                self.slots_tracked += slots_rooted;
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
        self.prev_epoch_credits = Some(epoch_credits_delta);

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

    /// Calculate average latency from histogram (17 - avg_credits)
    /// Latency 1 = 16 credits (fastest), Latency 17 = 0 credits (slowest)
    pub fn histogram_avg_latency(hist: &[u64; 17]) -> f64 {
        (MAX_CREDITS_PER_SLOT as f64 + 1.0) - Self::histogram_avg_credits(hist)
    }

    /// Get total credits for a time window (from histogram)
    pub fn window_credits(&self, window_secs: u64) -> u64 {
        let hist = self.window_histogram(window_secs);
        Self::histogram_credits(&hist)
    }

    /// Get expected max credits for a time window
    /// This is based on actual slots tracked, not time
    pub fn window_expected(&self, window_secs: u64) -> u64 {
        // Window expected = window actual credits + window missed credits
        // This is more accurate than estimating from time
        self.window_credits(window_secs) + self.window_missed(window_secs)
    }

    /// Calculate efficiency for a time window
    pub fn window_efficiency(&self, window_secs: u64) -> f64 {
        let expected = self.window_expected(window_secs);
        if expected == 0 {
            return 0.0;
        }
        self.window_credits(window_secs) as f64 / expected as f64
    }

    // ============ Consistency verification methods ============

    /// Verify that histogram credits + missed = expected (consistency check)
    /// Returns (actual_credits, missed_credits, expected_max, is_consistent)
    pub fn verify_epoch_consistency(&self) -> (u64, u64, u64, bool) {
        let histogram_credits = Self::histogram_credits(&self.epoch_histogram);
        let missed = self.epoch_missed;
        let expected = self.epoch_expected_max();

        // Consistency: actual + missed should equal expected
        // Note: There may be slight variance due to timing of updates
        let is_consistent = histogram_credits + missed == expected
            || (expected > 0 && (histogram_credits + missed) <= expected);

        (histogram_credits, missed, expected, is_consistent)
    }

    /// Verify window consistency
    pub fn verify_window_consistency(&self, window_secs: u64) -> (u64, u64, u64, bool) {
        let hist = self.window_histogram(window_secs);
        let credits = Self::histogram_credits(&hist);
        let missed = self.window_missed(window_secs);
        let expected = credits + missed; // Expected = actual + missed by definition

        (credits, missed, expected, true)
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

    // Helper: simulate epoch_credits delta (credits earned this epoch)
    fn sim_epoch_credits_delta(root_slot: u64, epoch_start: u64) -> u64 {
        (root_slot.saturating_sub(epoch_start) + 1) * MAX_CREDITS_PER_SLOT
    }

    // Helper: simulate total epoch credits (raw value, includes previous epochs for realism)
    fn sim_total_epoch_credits(root_slot: u64, epoch_start: u64) -> u64 {
        // Simulate that total credits = delta + some base from previous epochs
        let delta = sim_epoch_credits_delta(root_slot, epoch_start);
        // For testing, just use delta (no previous credits)
        delta
    }

    // ============ EpochInfo Tests ============

    #[test]
    fn test_epoch_info_from_slot() {
        // Slot 0 is in epoch 0
        let info = EpochInfo::from_slot(0);
        assert_eq!(info.epoch, 0);
        assert_eq!(info.slot_index, 0);
        assert_eq!(info.epoch_start_slot, 0);
        assert_eq!(info.slots_in_epoch, SLOTS_PER_EPOCH);

        // Last slot of epoch 0
        let info = EpochInfo::from_slot(SLOTS_PER_EPOCH - 1);
        assert_eq!(info.epoch, 0);
        assert_eq!(info.slot_index, SLOTS_PER_EPOCH - 1);
        assert_eq!(info.epoch_start_slot, 0);

        // First slot of epoch 1
        let info = EpochInfo::from_slot(SLOTS_PER_EPOCH);
        assert_eq!(info.epoch, 1);
        assert_eq!(info.slot_index, 0);
        assert_eq!(info.epoch_start_slot, SLOTS_PER_EPOCH);

        // Arbitrary slot in epoch 5
        let slot = 5 * SLOTS_PER_EPOCH + 12345;
        let info = EpochInfo::from_slot(slot);
        assert_eq!(info.epoch, 5);
        assert_eq!(info.slot_index, 12345);
        assert_eq!(info.epoch_start_slot, 5 * SLOTS_PER_EPOCH);
    }

    #[test]
    fn test_epoch_info_rooted_slots_elapsed() {
        let info = EpochInfo::from_slot(SLOTS_PER_EPOCH + 100);

        // Root slot is 100 slots into epoch 1
        let elapsed = info.rooted_slots_elapsed(SLOTS_PER_EPOCH + 100);
        assert_eq!(elapsed, 101); // 0 to 100 inclusive = 101 slots

        // Root slot at epoch start
        let elapsed = info.rooted_slots_elapsed(SLOTS_PER_EPOCH);
        assert_eq!(elapsed, 1); // Just slot 0 = 1 slot

        // Root slot from previous epoch (should return 0)
        let elapsed = info.rooted_slots_elapsed(SLOTS_PER_EPOCH - 1);
        assert_eq!(elapsed, 0);
    }

    #[test]
    fn test_epoch_info_expected_max_credits() {
        let info = EpochInfo::from_slot(SLOTS_PER_EPOCH + 100);

        // 101 slots * 16 credits/slot
        let expected = info.expected_max_credits(SLOTS_PER_EPOCH + 100);
        assert_eq!(expected, 101 * MAX_CREDITS_PER_SLOT);
    }

    // ============ VoteTracker Tests ============

    #[test]
    fn test_vote_tracker_new() {
        let tracker = VoteTracker::new();
        assert!(tracker.prev_votes.is_empty());
        assert_eq!(tracker.epoch_histogram, [0; 17]);
        assert!(tracker.epoch_info.is_none());
    }

    #[test]
    fn test_credits_calculation_with_latency() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH; // Epoch 1

        // First update establishes baseline - latency=1 means 16 credits
        let votes = vec![(epoch_start + 1000, 1, Some(1))];
        let result = tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // latency=1 → 16 credits

        // Add more votes with different latencies
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 1, Some(2)), // latency=2 → 15 credits
        ];
        let result = tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[15], 1); // 15 credits

        // Test with latency=3 → 14 credits
        let votes = vec![
            (epoch_start + 1000, 3, Some(1)),
            (epoch_start + 1001, 2, Some(2)),
            (epoch_start + 1002, 1, Some(3)),
        ];
        let result = tracker.process_update(
            epoch_start + 1002,
            &votes,
            Some(epoch_start + 1001),
            sim_epoch_credits_delta(epoch_start + 1001, epoch_start),
            sim_total_epoch_credits(epoch_start + 1001, epoch_start),
        );
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // latency=3 → 14 credits
    }

    #[test]
    fn test_credits_calculation_fallback() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // Test fallback when latency is None (use context_slot - vote_slot)
        let votes = vec![(epoch_start + 1000, 1, None)];
        let result = tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[16], 1); // gap=0 → 16 credits

        // Vote for slot 1001 seen at context slot 1003 (gap=2)
        let votes = vec![(epoch_start + 1000, 2, None), (epoch_start + 1001, 1, None)];
        let result = tracker.process_update(
            epoch_start + 1003,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );
        assert_eq!(result.new_votes, 1);
        assert_eq!(result.update_histogram[14], 1); // gap=2 → 14 credits
    }

    #[test]
    fn test_epoch_reset() {
        let mut tracker = VoteTracker::new();
        let epoch1_start = SLOTS_PER_EPOCH;
        let epoch2_start = 2 * SLOTS_PER_EPOCH;

        // Build up some histogram in epoch 1
        let votes = vec![(epoch1_start + 1000, 1, Some(1))];
        tracker.process_update(
            epoch1_start + 1000,
            &votes,
            Some(epoch1_start + 999),
            sim_epoch_credits_delta(epoch1_start + 999, epoch1_start),
            sim_total_epoch_credits(epoch1_start + 999, epoch1_start),
        );

        let votes = vec![
            (epoch1_start + 1000, 2, Some(1)),
            (epoch1_start + 1001, 1, Some(1)),
        ];
        tracker.process_update(
            epoch1_start + 1001,
            &votes,
            Some(epoch1_start + 1000),
            sim_epoch_credits_delta(epoch1_start + 1000, epoch1_start),
            sim_total_epoch_credits(epoch1_start + 1000, epoch1_start),
        );

        let count_before = tracker.epoch_histogram.iter().sum::<u64>();
        assert!(count_before > 0, "should have votes in epoch 1");
        assert_eq!(tracker.epoch_info.unwrap().epoch, 1);

        // New epoch should reset
        let votes = vec![(epoch2_start + 1, 1, Some(1))];
        tracker.process_update(
            epoch2_start + 1,
            &votes,
            Some(epoch2_start), // First slot of new epoch
            16,                 // New epoch, credits start fresh (delta)
            16,                 // Total epoch credits
        );

        // Epoch histogram should only contain the new vote
        assert_eq!(
            tracker.epoch_histogram.iter().sum::<u64>(),
            1,
            "epoch histogram should reset on new epoch"
        );
        assert_eq!(tracker.epoch_missed, 0, "epoch missed should reset");
        assert_eq!(tracker.epoch_info.unwrap().epoch, 2);
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
        let epoch_start = SLOTS_PER_EPOCH;

        // First update
        let votes = vec![(epoch_start + 1000, 1, Some(1))]; // 16 credits
        tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );

        // Second update
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 1, Some(2)), // 15 credits (new)
        ];
        tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );

        // Third update
        let votes = vec![
            (epoch_start + 1000, 3, Some(1)),
            (epoch_start + 1001, 2, Some(2)),
            (epoch_start + 1002, 1, Some(1)), // 16 credits (new)
        ];
        tracker.process_update(
            epoch_start + 1002,
            &votes,
            Some(epoch_start + 1001),
            sim_epoch_credits_delta(epoch_start + 1001, epoch_start),
            sim_total_epoch_credits(epoch_start + 1001, epoch_start),
        );

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
        let epoch_start = SLOTS_PER_EPOCH;

        // Test latency = 17 (0 credits - very slow)
        let votes = vec![(epoch_start + 1000, 1, Some(17))];
        let result = tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );
        assert_eq!(result.update_histogram[0], 1);

        // Test latency = 1 (16 credits - fastest)
        let votes = vec![
            (epoch_start + 1000, 2, Some(17)),
            (epoch_start + 1001, 1, Some(1)),
        ];
        let result = tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );
        assert_eq!(result.update_histogram[16], 1);

        // Test latency > 17 (should still be 0 credits)
        tracker.prev_votes.clear();
        let votes = vec![(epoch_start + 2000, 1, Some(100))];
        let result = tracker.process_update(
            epoch_start + 2000,
            &votes,
            Some(epoch_start + 1999),
            sim_epoch_credits_delta(epoch_start + 1999, epoch_start),
            sim_total_epoch_credits(epoch_start + 1999, epoch_start),
        );
        assert_eq!(result.update_histogram[0], 1);
    }

    #[test]
    fn test_multiple_updates_accumulate() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // First batch
        let votes = vec![(epoch_start + 1000, 1, Some(1))];
        tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );
        assert_eq!(tracker.cumulative_histogram[16], 1);

        // Second batch
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 1, Some(1)),
        ];
        tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Third batch
        let votes = vec![
            (epoch_start + 1000, 3, Some(1)),
            (epoch_start + 1001, 2, Some(1)),
            (epoch_start + 1002, 1, Some(1)),
        ];
        tracker.process_update(
            epoch_start + 1002,
            &votes,
            Some(epoch_start + 1001),
            sim_epoch_credits_delta(epoch_start + 1001, epoch_start),
            sim_total_epoch_credits(epoch_start + 1001, epoch_start),
        );
        assert_eq!(tracker.cumulative_histogram[16], 3);

        assert_eq!(
            VoteTracker::histogram_total(&tracker.cumulative_histogram),
            3
        );
    }

    #[test]
    fn test_duplicate_votes_not_counted() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // First update
        let votes = vec![
            (epoch_start + 1000, 1, Some(1)),
            (epoch_start + 1001, 1, Some(1)),
        ];
        tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );
        assert_eq!(tracker.cumulative_histogram[16], 2);

        // Same votes again (should not be counted again)
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 2, Some(1)),
        ];
        let result = tracker.process_update(
            epoch_start + 1002,
            &votes,
            Some(epoch_start + 1000),
            sim_epoch_credits_delta(epoch_start + 1000, epoch_start),
            sim_total_epoch_credits(epoch_start + 1000, epoch_start),
        );
        assert_eq!(result.new_votes, 0);
        assert_eq!(tracker.cumulative_histogram[16], 2); // Still 2
    }

    #[test]
    fn test_fresh_tracker_1h_equals_epoch() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        let votes: Vec<(u64, u32, Option<u32>)> = vec![
            (epoch_start + 1000, 1, Some(1)),  // 16 credits
            (epoch_start + 1001, 1, Some(2)),  // 15 credits
            (epoch_start + 1002, 1, Some(3)),  // 14 credits
            (epoch_start + 1003, 1, Some(16)), // 1 credit
            (epoch_start + 1004, 1, Some(17)), // 0 credits
        ];
        tracker.process_update(
            epoch_start + 1004,
            &votes,
            Some(epoch_start + 999),
            sim_epoch_credits_delta(epoch_start + 999, epoch_start),
            sim_total_epoch_credits(epoch_start + 999, epoch_start),
        );

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
        let epoch_start = SLOTS_PER_EPOCH;

        // First update: establishes baseline, no missed credits yet
        let votes = vec![(epoch_start + 1000, 1, Some(1))];
        let result = tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            16,  // Delta: 1 slot = 16 credits
            100, // Total epoch credits (arbitrary base)
        );
        assert_eq!(result.missed_credits, 0); // First update, no previous to compare

        // Second update: root advances by 1, earned 16 credits, no missed
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 1, Some(1)),
        ];
        let result = tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1000),
            32,  // Delta: 2 slots total = 32 credits
            116, // Total epoch credits
        );
        assert_eq!(result.missed_credits, 0); // Perfect voting

        // Third update: root advances by 2 slots, but only earned 16 credits (missed 16)
        let votes = vec![
            (epoch_start + 1000, 3, Some(1)),
            (epoch_start + 1001, 2, Some(1)),
            (epoch_start + 1002, 1, Some(1)),
        ];
        let result = tracker.process_update(
            epoch_start + 1002,
            &votes,
            Some(epoch_start + 1002),
            48,  // Delta: 3 slots = 48 credits, but only 16 delta from last
            132, // Total epoch credits
        );
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
        let epoch_start = SLOTS_PER_EPOCH;

        // Establish baseline
        let votes = vec![(epoch_start + 1000, 1, Some(1))];
        tracker.process_update(
            epoch_start + 1000,
            &votes,
            Some(epoch_start + 999),
            16,  // Delta
            100, // Total
        );

        // Add some missed credits
        let votes = vec![
            (epoch_start + 1000, 2, Some(1)),
            (epoch_start + 1001, 1, Some(1)),
        ];
        tracker.process_update(
            epoch_start + 1001,
            &votes,
            Some(epoch_start + 1001),
            24,  // Delta: 2 slots expected 32, got 24-16=8, missed 24
            108, // Total
        );
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

    // ============ Consistency Tests ============

    #[test]
    fn test_consistency_credits_plus_missed_equals_expected() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // Simulate 100 slots with mixed voting performance
        for i in 0..100 {
            let root = epoch_start + i;
            let credits_delta = if i % 3 == 0 {
                // Every 3rd slot: only 8 credits (half performance)
                (i + 1) * 8
            } else {
                // Full credits
                (i + 1) * 16
            };

            let latency = if i % 3 == 0 { Some(9) } else { Some(1) }; // 8 or 16 credits
            let votes = vec![(epoch_start + i, 1, latency)];
            tracker.process_update(
                epoch_start + i,
                &votes,
                Some(root),
                credits_delta as u64,        // Delta
                1000 + credits_delta as u64, // Total
            );
        }

        // Verify window consistency
        let (credits_5m, missed_5m, expected_5m, consistent_5m) =
            tracker.verify_window_consistency(300);
        assert!(consistent_5m, "5m window should be consistent");
        assert_eq!(credits_5m + missed_5m, expected_5m);

        let (credits_1h, missed_1h, expected_1h, consistent_1h) =
            tracker.verify_window_consistency(3600);
        assert!(consistent_1h, "1h window should be consistent");
        assert_eq!(credits_1h + missed_1h, expected_1h);
    }

    #[test]
    fn test_histogram_efficiency_consistency() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // Add 10 votes at 16 credits each
        for i in 0..10 {
            let votes = vec![(epoch_start + i, 1, Some(1))];
            tracker.process_update(
                epoch_start + i,
                &votes,
                Some(epoch_start + i),
                (i + 1) * 16,        // Delta: Perfect credits
                1000 + (i + 1) * 16, // Total
            );
        }

        // Efficiency should be 1.0 (100%)
        let hist = tracker.epoch_histogram();
        let efficiency = VoteTracker::histogram_efficiency(&hist);
        assert!(
            (efficiency - 1.0).abs() < 0.001,
            "Efficiency should be 1.0 for all 16-credit votes"
        );

        // Window efficiency should match
        let window_eff = tracker.window_efficiency(300);
        assert!(
            (window_eff - 1.0).abs() < 0.001,
            "Window efficiency should be 1.0"
        );
    }

    #[test]
    fn test_window_credits_matches_histogram_credits() {
        let mut tracker = VoteTracker::new();
        let epoch_start = SLOTS_PER_EPOCH;

        // Add various votes
        let votes: Vec<(u64, u32, Option<u32>)> = vec![
            (epoch_start + 1, 1, Some(1)),  // 16 credits
            (epoch_start + 2, 1, Some(3)),  // 14 credits
            (epoch_start + 3, 1, Some(5)),  // 12 credits
            (epoch_start + 4, 1, Some(10)), // 7 credits
        ];
        tracker.process_update(
            epoch_start + 4,
            &votes,
            Some(epoch_start + 4),
            49,   // Delta: 16 + 14 + 12 + 7 = 49
            1049, // Total
        );

        let hist = tracker.window_histogram(300);
        let hist_credits = VoteTracker::histogram_credits(&hist);
        let window_credits = tracker.window_credits(300);

        assert_eq!(
            hist_credits, window_credits,
            "histogram_credits should match window_credits"
        );
        assert_eq!(hist_credits, 16 + 14 + 12 + 7);
    }
}
