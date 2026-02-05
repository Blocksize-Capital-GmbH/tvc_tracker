use anyhow::Result;
use axum::http::{HeaderMap, HeaderValue};
use prometheus::{Encoder, IntCounter, IntGauge, Opts, Registry, TextEncoder};
use std::sync::Arc;

pub const MAX_CREDITS_PER_SLOT: u64 = 16;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,
    pub expected_max: IntGauge,
    pub actual: IntGauge,
    pub missed_current_epoch: IntGauge,
    pub missed_last_epoch: IntGauge,
    pub missed_since_last_poll: IntGauge,
    pub missed_5m: IntGauge,
    pub missed_1h: IntGauge,
    pub missed_total: IntCounter,
    pub rpc_up: IntGauge,
    pub rpc_errors: IntCounter,
    pub rpc_last_success: IntGauge,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let expected_max = IntGauge::with_opts(Opts::new(
            "solana_vote_credits_expected_max",
            "Theoretical max vote credits earned so far this epoch (max credits per slot)",
        ))?;

        let actual = IntGauge::with_opts(Opts::new(
            "solana_vote_credits_actual",
            "Actual vote credits earned so far this epoch (from vote account epochCredits)",
        ))?;

        let missed_current_epoch = IntGauge::with_opts(Opts::new(
            "missed_vote_credits_current_epoch",
            "Number of timely vote credits missed this epoch",
        ))?;

        let missed_last_epoch = IntGauge::with_opts(Opts::new(
            "missed_vote_credits_last_epoch",
            "Number of timely vote credits missed last epoch",
        ))?;

        let missed_since_last_poll = IntGauge::with_opts(Opts::new(
            "missed_vote_credits_since_last_poll",
            "Number of timely vote credits missed since the last poll",
        ))?;

        let missed_5m = IntGauge::with_opts(Opts::new(
            "missed_vote_credits_5m",
            "Number of timely vote credits missed the past 5 minutes",
        ))?;

        let missed_1h = IntGauge::with_opts(Opts::new(
            "missed_vote_credits_1h",
            "Number of timely vote credits missed the past 5 minutes",
        ))?;

        let missed_total = IntCounter::with_opts(Opts::new(
            "missed_vote_credits_total",
            "Number of timely vote credits missed total",
        ))?;

        let rpc_up = IntGauge::with_opts(Opts::new(
            "rpc_up",
            "Is 0 when the RPC is unavailable and 1 if it's available.",
        ))?;

        let rpc_errors = IntCounter::with_opts(Opts::new(
            "rpc_errors",
            "Counts the number of RPC request errors.",
        ))?;

        let rpc_last_success = IntGauge::with_opts(Opts::new(
            "rpc_last_success",
            "Timestamp of the last successful RPC request.",
        ))?;

        registry.register(Box::new(expected_max.clone()))?;
        registry.register(Box::new(actual.clone()))?;
        registry.register(Box::new(missed_current_epoch.clone()))?;
        registry.register(Box::new(missed_last_epoch.clone()))?;
        registry.register(Box::new(missed_since_last_poll.clone()))?;
        registry.register(Box::new(missed_5m.clone()))?;
        registry.register(Box::new(missed_1h.clone()))?;
        registry.register(Box::new(missed_total.clone()))?;
        registry.register(Box::new(rpc_up.clone()))?;
        registry.register(Box::new(rpc_errors.clone()))?;
        registry.register(Box::new(rpc_last_success.clone()))?;

        Ok(Self {
            registry,
            expected_max,
            actual,
            missed_current_epoch,
            missed_last_epoch,
            missed_since_last_poll,
            missed_5m,
            missed_1h,
            missed_total,
            rpc_up,
            rpc_errors,
            rpc_last_success,
        })
    }

    pub fn render(&self) -> (HeaderMap, String) {
        let families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).expect("encode metrics");
        let body = String::from_utf8(buf).expect("utf8 metrics");

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_str(encoder.format_type()).unwrap(),
        );
        (headers, body)
    }
}

pub async fn metrics_handler(metrics: Arc<Metrics>) -> (HeaderMap, String) {
    metrics.render()
}
