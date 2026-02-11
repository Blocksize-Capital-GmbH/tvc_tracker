use anyhow::Result;
use axum::http::{HeaderMap, HeaderValue};
use prometheus::{
    Encoder, Gauge, GaugeVec, IntCounter, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::sync::Arc;

pub const MAX_CREDITS_PER_SLOT: u64 = 16;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,
    // Epoch info
    pub epoch: IntGauge,
    pub slot_index: IntGauge,
    // Credits
    pub expected_max: IntGauge,
    pub actual: IntGauge,
    pub missed_current_epoch: IntGauge,
    pub missed_last_epoch: IntGauge,
    pub missed_since_last_poll: IntGauge,
    pub missed_5m: IntGauge,
    pub missed_1h: IntGauge,
    pub missed_rate_5m: Gauge,
    pub missed_rate_1h: Gauge,
    pub vote_credits_efficiency_5m: Gauge,
    pub vote_credits_efficiency_1h: Gauge,
    pub vote_credits_efficiency_epoch: Gauge,
    pub vote_credits_per_slot_5m: Gauge,
    pub vote_credits_per_slot_1h: Gauge,
    pub vote_credits_per_slot_epoch: Gauge,
    pub vote_latency_slots_5m: Gauge,
    pub vote_latency_slots_1h: Gauge,
    pub vote_latency_slots_epoch: Gauge,
    /// Histogram: vote count by credits earned (0-16) per window (5m, 1h, epoch)
    pub vote_credits_histogram_count: IntGaugeVec,
    /// Histogram: relative fraction by credits earned (0-16) per window
    pub vote_credits_histogram_fraction: GaugeVec,
    pub missed_total: IntCounter,
    pub projected_credits_epoch: IntGauge,
    pub rpc_up: IntGauge,
    pub rpc_errors: IntCounter,
    pub rpc_last_success: IntGauge,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let epoch = IntGauge::with_opts(Opts::new(
            "solana_epoch",
            "Current epoch number (derived from root slot)",
        ))?;

        let slot_index = IntGauge::with_opts(Opts::new(
            "solana_slot_index",
            "Current slot index within the epoch (0 to 431999)",
        ))?;

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
            "Number of timely vote credits missed the past 1 hour",
        ))?;

        let missed_rate_5m = Gauge::with_opts(Opts::new(
            "missed_vote_credits_rate_5m",
            "Rate of missed vote credits per minute (5-minute average)",
        ))?;

        let missed_rate_1h = Gauge::with_opts(Opts::new(
            "missed_vote_credits_rate_1h",
            "Rate of missed vote credits per minute (1-hour average)",
        ))?;

        let vote_credits_efficiency_5m = Gauge::with_opts(Opts::new(
            "solana_vote_credits_efficiency_5m",
            "Fraction of max vote credits earned (5-minute window, 1.0 = 100%)",
        ))?;

        let vote_credits_efficiency_1h = Gauge::with_opts(Opts::new(
            "solana_vote_credits_efficiency_1h",
            "Fraction of max vote credits earned (1-hour window, 1.0 = 100%)",
        ))?;

        let vote_credits_efficiency_epoch = Gauge::with_opts(Opts::new(
            "solana_vote_credits_efficiency_epoch",
            "Fraction of max vote credits earned this epoch (actual/expected, 1.0 = 100%)",
        ))?;

        let vote_credits_per_slot_5m = Gauge::with_opts(Opts::new(
            "solana_vote_credits_per_slot_5m",
            "Average vote credits earned per slot (5-minute window, max 16)",
        ))?;

        let vote_credits_per_slot_1h = Gauge::with_opts(Opts::new(
            "solana_vote_credits_per_slot_1h",
            "Average vote credits earned per slot (1-hour window, max 16)",
        ))?;

        let vote_credits_per_slot_epoch = Gauge::with_opts(Opts::new(
            "solana_vote_credits_per_slot_epoch",
            "Average vote credits earned per slot this epoch (max 16)",
        ))?;

        let vote_latency_slots_5m = Gauge::with_opts(Opts::new(
            "solana_vote_latency_slots_5m",
            "Implied average vote latency in slots (5-minute window, 1 = fastest)",
        ))?;

        let vote_latency_slots_1h = Gauge::with_opts(Opts::new(
            "solana_vote_latency_slots_1h",
            "Implied average vote latency in slots (1-hour window, 1 = fastest)",
        ))?;

        let vote_latency_slots_epoch = Gauge::with_opts(Opts::new(
            "solana_vote_latency_slots_epoch",
            "Implied average vote latency in slots this epoch (1 = fastest)",
        ))?;

        let missed_total = IntCounter::with_opts(Opts::new(
            "missed_vote_credits_total",
            "Number of timely vote credits missed total",
        ))?;

        let projected_credits_epoch = IntGauge::with_opts(Opts::new(
            "solana_vote_credits_projected_epoch",
            "Projected total vote credits by the end of the epoch (based on current rate)",
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

        // Histogram metrics with labels: window (5m, 1h, epoch), credits (0-16)
        let vote_credits_histogram_count = IntGaugeVec::new(
            Opts::new(
                "solana_vote_credits_histogram_count",
                "Number of votes earning each credit value (0-16) per window",
            ),
            &["window", "credits"],
        )?;

        let vote_credits_histogram_fraction = GaugeVec::new(
            Opts::new(
                "solana_vote_credits_histogram_fraction",
                "Fraction of votes earning each credit value (0-16) per window",
            ),
            &["window", "credits"],
        )?;

        registry.register(Box::new(epoch.clone()))?;
        registry.register(Box::new(slot_index.clone()))?;
        registry.register(Box::new(expected_max.clone()))?;
        registry.register(Box::new(actual.clone()))?;
        registry.register(Box::new(missed_current_epoch.clone()))?;
        registry.register(Box::new(missed_last_epoch.clone()))?;
        registry.register(Box::new(missed_since_last_poll.clone()))?;
        registry.register(Box::new(missed_5m.clone()))?;
        registry.register(Box::new(missed_1h.clone()))?;
        registry.register(Box::new(missed_rate_5m.clone()))?;
        registry.register(Box::new(missed_rate_1h.clone()))?;
        registry.register(Box::new(vote_credits_efficiency_5m.clone()))?;
        registry.register(Box::new(vote_credits_efficiency_1h.clone()))?;
        registry.register(Box::new(vote_credits_efficiency_epoch.clone()))?;
        registry.register(Box::new(vote_credits_per_slot_5m.clone()))?;
        registry.register(Box::new(vote_credits_per_slot_1h.clone()))?;
        registry.register(Box::new(vote_credits_per_slot_epoch.clone()))?;
        registry.register(Box::new(vote_latency_slots_5m.clone()))?;
        registry.register(Box::new(vote_latency_slots_1h.clone()))?;
        registry.register(Box::new(vote_latency_slots_epoch.clone()))?;
        registry.register(Box::new(missed_total.clone()))?;
        registry.register(Box::new(projected_credits_epoch.clone()))?;
        registry.register(Box::new(rpc_up.clone()))?;
        registry.register(Box::new(rpc_errors.clone()))?;
        registry.register(Box::new(rpc_last_success.clone()))?;
        registry.register(Box::new(vote_credits_histogram_count.clone()))?;
        registry.register(Box::new(vote_credits_histogram_fraction.clone()))?;

        Ok(Self {
            registry,
            epoch,
            slot_index,
            expected_max,
            actual,
            missed_current_epoch,
            missed_last_epoch,
            missed_since_last_poll,
            missed_5m,
            missed_1h,
            missed_rate_5m,
            missed_rate_1h,
            vote_credits_efficiency_5m,
            vote_credits_efficiency_1h,
            vote_credits_efficiency_epoch,
            vote_credits_per_slot_5m,
            vote_credits_per_slot_1h,
            vote_credits_per_slot_epoch,
            vote_latency_slots_5m,
            vote_latency_slots_1h,
            vote_latency_slots_epoch,
            vote_credits_histogram_count,
            vote_credits_histogram_fraction,
            missed_total,
            projected_credits_epoch,
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
