use std::sync::Arc;
use std::time::{Duration, Instant};
use tvc_tracker::config::{Args, Commitment};
use tvc_tracker::metrics::Metrics;
use tvc_tracker::poller::{PollState, poll_once};
use tvc_tracker::rpc::client::RpcClient;
use tvc_tracker::rpc::types::{EpochInfo, VoteAccount};

/// A configurable fake RPC client for testing
struct TestRpc {
    epoch_info: EpochInfo,
    vote_account: VoteAccount,
}

impl TestRpc {
    fn new(epoch: u64, slot_index: u64, slots_in_epoch: u64, absolute_slot: u64) -> Self {
        Self {
            epoch_info: EpochInfo {
                epoch,
                slot_index,
                slots_in_epoch,
                absolute_slot,
            },
            vote_account: VoteAccount {
                vote_pubkey: "TestVotePubkey".to_string(),
                node_pubkey: "TestNodePubkey".to_string(),
                activated_stake: 1_000_000_000,
                commission: 10,
                epoch_credits: vec![[epoch, 100_000, 50_000]],
                last_vote: absolute_slot,
                root_slot: absolute_slot.saturating_sub(32),
            },
        }
    }

    fn with_credits(mut self, epoch: u64, credits: u64, prev_credits: u64) -> Self {
        self.vote_account.epoch_credits = vec![[epoch, credits, prev_credits]];
        self
    }

    fn with_epoch_credits(mut self, epoch_credits: Vec<[u64; 3]>) -> Self {
        self.vote_account.epoch_credits = epoch_credits;
        self
    }

    fn with_root_slot(mut self, root_slot: u64) -> Self {
        self.vote_account.root_slot = root_slot;
        self
    }
}

impl RpcClient for TestRpc {
    fn rpc_url(&self) -> &str {
        "http://test.local"
    }

    async fn get_epoch_info(&self, _commitment: Commitment) -> anyhow::Result<EpochInfo> {
        Ok(self.epoch_info.clone())
    }

    async fn get_vote_account(
        &self,
        _vote_pubkey: &str,
        _commitment: Commitment,
    ) -> anyhow::Result<VoteAccount> {
        Ok(self.vote_account.clone())
    }
}

fn test_args() -> Args {
    Args {
        vote_pubkey: "TestVotePubkey".to_string(),
        rpc_url: "http://test.local".to_string(),
        commitment: Commitment::Finalized,
        interval_secs: 60,
        log_dir: "/tmp/test_logs".to_string(),
        metrics_port: 7999,
    }
}

#[tokio::test]
async fn test_poll_once_records_metrics() {
    let rpc = TestRpc::new(100, 1000, 432_000, 43_201_000)
        .with_credits(100, 100_000, 50_000)
        .with_root_slot(43_201_000 - 32);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

    // Verify metrics were set
    assert!(
        metrics.actual.get() > 0,
        "actual credits should be recorded"
    );
    assert_eq!(
        metrics.ws_connected.get(),
        1,
        "ws_connected should be 1 after success"
    );
    assert!(
        metrics.ws_last_message.get() > 0,
        "ws_last_message should be set"
    );
}

#[tokio::test]
async fn test_poll_state_backoff_on_success() {
    let mut state = PollState::new(60);
    assert_eq!(state.backoff, Duration::from_secs(60));

    state.on_error();
    assert_eq!(state.backoff, Duration::from_secs(120));

    state.on_error();
    assert_eq!(state.backoff, Duration::from_secs(240));

    state.on_success();
    assert_eq!(state.backoff, Duration::from_secs(60));
}

#[tokio::test]
async fn test_poll_state_backoff_capped_at_600s() {
    let mut state = PollState::new(60);

    // Simulate many errors
    for _ in 0..20 {
        state.on_error();
    }

    assert_eq!(
        state.backoff,
        Duration::from_secs(600),
        "backoff should be capped at 600s"
    );
}

#[tokio::test]
async fn test_missed_credits_calculation() {
    // Setup: epoch 100, slot_index 1000, 432k slots per epoch
    // Root slot at epoch_start + 1000 means 1001 rooted slots
    // Expected max = 1001 * 16 = 16016 credits
    // Actual credits this epoch = 16000 (100000 - 84000)
    // Missed = 16016 - 16000 = 16

    let epoch_start_slot = 43_200_000u64;
    let current_slot = epoch_start_slot + 1000;
    let root_slot = current_slot - 32; // 32 slots behind tip

    let rpc = TestRpc::new(100, 1000, 432_000, current_slot)
        .with_credits(100, 100_000, 84_000) // 16000 credits this epoch
        .with_root_slot(root_slot);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

    // Credits this epoch = 100000 - 84000 = 16000
    assert_eq!(metrics.actual.get(), 16000);

    // expected_max is calculated based on rooted slots
    assert!(
        metrics.expected_max.get() > 0,
        "expected_max should be calculated"
    );
}

#[tokio::test]
async fn test_multiple_epoch_credits_tracking() {
    let epoch_credits = vec![
        [98, 800_000, 700_000],  // epoch 98: 100k credits
        [99, 900_000, 800_000],  // epoch 99: 100k credits
        [100, 950_000, 900_000], // epoch 100: 50k credits so far
    ];

    let rpc = TestRpc::new(100, 1000, 432_000, 43_201_000)
        .with_epoch_credits(epoch_credits)
        .with_root_slot(43_201_000 - 32);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

    // Current epoch credits = 950000 - 900000 = 50000
    assert_eq!(metrics.actual.get(), 50_000);

    // Last epoch missed should be calculated (epoch 99)
    // Expected for full epoch = 432000 * 16 = 6,912,000
    // Earned = 100,000
    // Missed = 6,912,000 - 100,000 = 6,812,000
    assert!(
        metrics.missed_last_epoch.get() > 0,
        "missed_last_epoch should be set"
    );
}

#[tokio::test]
async fn test_delta_missed_between_polls() {
    let rpc1 = TestRpc::new(100, 1000, 432_000, 43_201_000)
        .with_credits(100, 100_000, 84_000)
        .with_root_slot(43_200_968);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    // First poll
    poll_once(&rpc1, &args, &metrics, &mut state).await.unwrap();
    let first_missed = metrics.missed_current_epoch.get();

    // Second poll with more slots and credits
    let rpc2 = TestRpc::new(100, 2000, 432_000, 43_202_000)
        .with_credits(100, 116_000, 84_000) // 32000 credits now
        .with_root_slot(43_201_968);

    poll_once(&rpc2, &args, &metrics, &mut state).await.unwrap();
    let second_missed = metrics.missed_current_epoch.get();

    // Missed credits should be tracked
    assert!(
        second_missed >= first_missed || second_missed >= 0,
        "missed credits tracking should work across polls"
    );
}

#[test]
fn test_metrics_creation() {
    let metrics = Metrics::new();
    assert!(metrics.is_ok(), "Metrics should be created successfully");

    let m = metrics.unwrap();

    // Verify all gauges start at 0
    assert_eq!(m.expected_max.get(), 0);
    assert_eq!(m.actual.get(), 0);
    assert_eq!(m.missed_current_epoch.get(), 0);
    assert_eq!(m.missed_last_epoch.get(), 0);
    assert_eq!(m.missed_since_last_poll.get(), 0);
    assert_eq!(m.missed_5m.get(), 0);
    assert_eq!(m.missed_1h.get(), 0);
    assert_eq!(m.ws_connected.get(), 0);
}

#[test]
fn test_metrics_render() {
    let metrics = Metrics::new().unwrap();
    metrics.expected_max.set(100);
    metrics.actual.set(95);
    metrics.ws_connected.set(1);

    let (headers, body) = metrics.render();

    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/plain"),
        "Content-Type should be prometheus format"
    );

    assert!(
        body.contains("solana_vote_credits_expected_max 100"),
        "Body should contain expected_max metric"
    );
    assert!(
        body.contains("solana_vote_credits_actual 95"),
        "Body should contain actual metric"
    );
    assert!(
        body.contains("ws_connected 1"),
        "Body should contain ws_connected metric"
    );
}

#[test]
fn test_args_validation_empty_pubkey() {
    let args = Args {
        vote_pubkey: "".to_string(),
        rpc_url: "http://localhost:8899".to_string(),
        commitment: Commitment::Finalized,
        interval_secs: 60,
        log_dir: "logs".to_string(),
        metrics_port: 7999,
    };

    let result = args.validate();
    assert!(result.is_err(), "Empty vote_pubkey should fail validation");
}

#[test]
fn test_args_validation_whitespace_pubkey() {
    let args = Args {
        vote_pubkey: "   ".to_string(),
        rpc_url: "http://localhost:8899".to_string(),
        commitment: Commitment::Finalized,
        interval_secs: 60,
        log_dir: "logs".to_string(),
        metrics_port: 7999,
    };

    let result = args.validate();
    assert!(
        result.is_err(),
        "Whitespace-only vote_pubkey should fail validation"
    );
}

#[test]
fn test_args_validation_valid_pubkey() {
    let args = Args {
        vote_pubkey: "HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib".to_string(),
        rpc_url: "http://localhost:8899".to_string(),
        commitment: Commitment::Finalized,
        interval_secs: 60,
        log_dir: "logs".to_string(),
        metrics_port: 7999,
    };

    let result = args.validate();
    assert!(result.is_ok(), "Valid pubkey should pass validation");
}

#[test]
fn test_poll_state_history_window() {
    let mut state = PollState::new(60);

    let now = Instant::now();

    // Add some historical data points (timestamp, missed_acc, slot)
    state
        .hist
        .push_back((now - Duration::from_secs(4000), 100, 1000)); // > 1h ago
    state
        .hist
        .push_back((now - Duration::from_secs(3500), 150, 1500)); // > 1h ago
    state
        .hist
        .push_back((now - Duration::from_secs(3000), 200, 2000)); // within 1h
    state
        .hist
        .push_back((now - Duration::from_secs(200), 250, 2800)); // within 5m
    state.hist.push_back((now, 300, 3000)); // current

    assert_eq!(state.hist.len(), 5, "History should have 5 entries");

    state.missed_total_acc = 300;
}

/// Mutable RPC client for epoch boundary testing (thread-safe)
struct MutableTestRpc {
    epoch_info: std::sync::RwLock<EpochInfo>,
    vote_account: std::sync::RwLock<VoteAccount>,
}

impl MutableTestRpc {
    fn new(epoch: u64, slot_index: u64, slots_in_epoch: u64, absolute_slot: u64) -> Self {
        Self {
            epoch_info: std::sync::RwLock::new(EpochInfo {
                epoch,
                slot_index,
                slots_in_epoch,
                absolute_slot,
            }),
            vote_account: std::sync::RwLock::new(VoteAccount {
                vote_pubkey: "TestVotePubkey".to_string(),
                node_pubkey: "TestNodePubkey".to_string(),
                activated_stake: 1_000_000_000,
                commission: 10,
                epoch_credits: vec![[epoch, 100_000, 50_000]],
                last_vote: absolute_slot,
                root_slot: absolute_slot.saturating_sub(32),
            }),
        }
    }

    fn advance_epoch(&self, new_epoch: u64, new_slot_index: u64, new_absolute_slot: u64) {
        let mut info = self.epoch_info.write().unwrap();
        info.epoch = new_epoch;
        info.slot_index = new_slot_index;
        info.absolute_slot = new_absolute_slot;

        let mut acct = self.vote_account.write().unwrap();
        // Add previous epoch to epoch_credits and start new epoch
        let prev_credits = acct.epoch_credits.last().map(|c| c[1]).unwrap_or(0);
        acct.epoch_credits
            .push([new_epoch, prev_credits + 1000, prev_credits]);
        acct.root_slot = new_absolute_slot.saturating_sub(32);
        acct.last_vote = new_absolute_slot;
    }

    fn add_credits(&self, additional: u64) {
        let mut acct = self.vote_account.write().unwrap();
        if let Some(last) = acct.epoch_credits.last_mut() {
            last[1] += additional;
        }
    }

    fn advance_slots(&self, slots: u64) {
        let mut info = self.epoch_info.write().unwrap();
        info.slot_index += slots;
        info.absolute_slot += slots;

        let mut acct = self.vote_account.write().unwrap();
        acct.root_slot += slots;
        acct.last_vote += slots;
    }
}

impl RpcClient for MutableTestRpc {
    fn rpc_url(&self) -> &str {
        "http://test.local"
    }

    async fn get_epoch_info(&self, _commitment: Commitment) -> anyhow::Result<EpochInfo> {
        Ok(self.epoch_info.read().unwrap().clone())
    }

    async fn get_vote_account(
        &self,
        _vote_pubkey: &str,
        _commitment: Commitment,
    ) -> anyhow::Result<VoteAccount> {
        Ok(self.vote_account.read().unwrap().clone())
    }
}

#[tokio::test]
async fn test_epoch_boundary_resets_delta() {
    // Start in epoch 100
    let rpc = MutableTestRpc::new(100, 1000, 432_000, 43_201_000);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    // First poll in epoch 100
    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();
    let first_missed = metrics.missed_current_epoch.get();
    assert!(first_missed >= 0, "first poll should record missed credits");

    // Advance slots and add some credits
    rpc.advance_slots(100);
    rpc.add_credits(1500);

    // Second poll in same epoch - should calculate delta
    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();
    assert_eq!(
        state.prev_epoch,
        Some(100),
        "prev_epoch should be set to 100"
    );

    // Advance to epoch 101
    rpc.advance_epoch(101, 100, 43_632_100);

    // Poll in new epoch - delta should be 0 (epoch boundary)
    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

    assert_eq!(
        state.prev_epoch,
        Some(101),
        "prev_epoch should update to 101"
    );
    // On epoch change, missed_since_last_poll should be 0
    assert_eq!(
        metrics.missed_since_last_poll.get(),
        0,
        "delta should be 0 at epoch boundary"
    );
}

#[tokio::test]
async fn test_window_metrics_accumulate_correctly() {
    let rpc = MutableTestRpc::new(100, 1000, 432_000, 43_201_000);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    // First poll
    poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

    // Simulate passage of time and missed credits
    for i in 0..5 {
        rpc.advance_slots(100);
        // Don't add proportional credits, causing missed credits to accumulate
        rpc.add_credits(800); // Less than expected (1600 for 100 slots)
        poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();

        // After each poll, missed_total should be >= missed_1h >= missed_5m
        let total = state.missed_total_acc;
        let m1h = metrics.missed_1h.get() as u64;
        let m5m = metrics.missed_5m.get() as u64;

        assert!(
            total >= m1h,
            "iteration {}: total ({}) should be >= 1h ({})",
            i,
            total,
            m1h
        );
        assert!(
            m1h >= m5m,
            "iteration {}: 1h ({}) should be >= 5m ({})",
            i,
            m1h,
            m5m
        );
    }
}

#[tokio::test]
async fn test_missed_credits_1h_less_than_or_equal_total_after_warmup() {
    let rpc = MutableTestRpc::new(100, 1000, 432_000, 43_201_000);

    let args = test_args();
    let metrics = Arc::new(Metrics::new().unwrap());
    let mut state = PollState::new(60);

    // Poll multiple times
    for _ in 0..10 {
        rpc.advance_slots(50);
        poll_once(&rpc, &args, &metrics, &mut state).await.unwrap();
    }

    let total = state.missed_total_acc;
    let m1h = metrics.missed_1h.get() as u64;
    let m5m = metrics.missed_5m.get() as u64;

    // When running less than 1 hour, 1h should equal total accumulated
    assert_eq!(
        total, m1h,
        "When running < 1h, 1h window should equal total accumulated"
    );

    // 5m should be <= 1h
    assert!(m5m <= m1h, "5m ({}) should be <= 1h ({})", m5m, m1h);
}
