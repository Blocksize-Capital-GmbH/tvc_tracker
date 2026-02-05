use tracing::info;
use tracing_subscriber::EnvFilter;
use std::time::{Instant};
use crate::poller::{CreditsSnapshot};

pub(crate) fn append_log(
    prev: Option<(&CreditsSnapshot, Instant)>, 
    cur: &CreditsSnapshot, 
    now: Instant
) {
    if let Some((p, t0)) = prev {
        let d_credits = cur.credits_total.saturating_sub(p.credits_total);
        let dt = now.duration_since(t0).as_secs_f64().max(0.001);
        let rate = (d_credits as f64) / dt;
        info!(
            "epoch={} credits_this_epoch={} (Δtotal={} | {:.2} credits/s)",
            cur.epoch, cur.credits_this_epoch, d_credits, rate
        );
    } else {
        info!(
            "epoch={} credits_this_epoch={} (total={} start={})",
            cur.epoch, cur.credits_this_epoch, cur.credits_total, cur.credits_at_epoch_start
        );
    }
}

pub fn init_logging(log_dir: &str) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    // daily rotating file: logs/tvc_tracker.YYYY-MM-DD
    let file_appender = tracing_appender::rolling::daily(log_dir, "tvc_tracker.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false) // log files don’t need rainbow control codes
        .init();

    Ok(guard) // keep this alive or logs may not flush
}