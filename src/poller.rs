use crate::rpc::client::RpcClient;
use crate::rpc::types::VoteAccount;
use crate::config::Args;
use crate::metrics::{MAX_CREDITS_PER_SLOT, Metrics};
use crate::logging::append_log;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct CreditsSnapshot {
    pub epoch: u64,
    pub credits_total: u64,
    pub credits_at_epoch_start: u64,
    pub credits_this_epoch: u64,
}

#[derive(Debug)]
pub struct PollState {
    pub prev: Option<(CreditsSnapshot, Instant)>,
    pub prev_missed: Option<u64>,
    pub missed_total_acc: u64,
    pub hist: VecDeque<(Instant, u64)>,
    pub base_interval: Duration,
    pub backoff: Duration,
}

impl PollState {
    pub fn new(interval_secs: u64) -> Self {
        let base = Duration::from_secs(interval_secs);
        Self {
            prev: None,
            prev_missed: None,
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

    let epoch_info = rpc.get_epoch_info(
            args.commitment
        ).await?;
    let acct = rpc.get_vote_account(
            &args.vote_pubkey, 
            args.commitment
        ).await?;
    let cur = snapshot_from_vote_account(&acct)?;


    m.rpc_up.set(1);
    m.rpc_last_success.set(
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

    // actual credits this epoch from epochCredits last tuple
    let last = acct
        .epoch_credits
        .last()
        .ok_or_else(|| anyhow!("epochCredits empty"))?;

    let credits_this_epoch = last[1].saturating_sub(last[2]);

    let missed_now = expected_max_rooted.saturating_sub(credits_this_epoch);

    let delta_missed = match state.prev_missed {
        Some(p) => missed_now.saturating_sub(p),
        None => 0,
    };

    state.prev_missed = Some(missed_now);
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

    state.hist.push_back((now, state.missed_total_acc));

    let cutoff = now - Duration::from_secs(3600);
    while let Some((t, _)) = state.hist.front() {
        if *t < cutoff {
            state.hist.pop_front();
        } else {
            break;
        }
    }

    let missed_5m =
        window_delta(
            &state.hist, 
            now, 
            Duration::from_secs(300), 
            state.missed_total_acc
        );
    let missed_1h =
        window_delta(
            &state.hist, 
            now, 
            Duration::from_secs(3600), 
            state.missed_total_acc
        );

    m.missed_5m.set(missed_5m as i64);
    m.missed_1h.set(missed_1h as i64);
    m.expected_max.set(expected_max_rooted as i64);
    m.actual.set(cur.credits_this_epoch as i64);
    m.missed_current_epoch.set(missed_now as i64);

    append_log(state.prev.as_ref().map(|(s, t)| (s, *t)), &cur, now);
    state.prev = Some((cur, now));

    Ok(())
}

pub async fn run_poll<C :RpcClient + Sync>(
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
                m.rpc_up.set(0);
                m.rpc_errors.inc();
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
