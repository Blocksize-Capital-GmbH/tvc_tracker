# TVC Tracker

<div align="center">

<img src="assets/solana_logo.png" height="60" alt="Solana">&nbsp;&nbsp;&nbsp;&nbsp;<img src="assets/blocksize_logo_white.png" height="60" alt="Blocksize">

**A high-performance Prometheus exporter for monitoring Solana validator Timely Vote Credits (TVC)**

![Solana](https://img.shields.io/badge/Solana-9945FF?style=for-the-badge&logo=solana&logoColor=white)
![Rust](https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white)
![Prometheus](https://img.shields.io/badge/Prometheus-E6522C?style=for-the-badge&logo=prometheus&logoColor=white)
![Docker](https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

</div>

---

## Quick Start

```bash
# Docker
docker run -d --name tvc_tracker --network host \
  ghcr.io/blocksize-capital-gmbh/tvc_tracker:latest \
  --vote-pubkey YOUR_VOTE_PUBKEY \
  --rpc-url https://api.mainnet-beta.solana.com

# From source
cargo build --release
./target/release/tvc_tracker --vote-pubkey YOUR_VOTE_PUBKEY
```

## Configuration

| Argument | Description | Default |
|----------|-------------|---------|
| `--vote-pubkey` | Vote account pubkey (base58) | **Required** |
| `--rpc-url` | Solana RPC endpoint | `https://api.mainnet.solana.com` |
| `--commitment` | `processed`, `confirmed`, `finalized` | `finalized` |
| `--interval-secs` | Polling interval (seconds) | `60` |
| `--metrics-port` | Prometheus metrics port | `7999` |
| `--log-dir` | Log file directory | `logs` |

## Metrics

### Aggregate Metrics (REST Polling)

| Metric | Type | Description |
|--------|------|-------------|
| `solana_vote_credits_expected_max` | Gauge | Max theoretical credits (slots × 16) |
| `solana_vote_credits_actual` | Gauge | Actual credits earned this epoch |
| `solana_vote_credits_projected_epoch` | Gauge | Projected total credits by epoch end |
| `missed_vote_credits_current_epoch` | Gauge | Credits missed this epoch |
| `missed_vote_credits_last_epoch` | Gauge | Credits missed last epoch |
| `missed_vote_credits_since_last_poll` | Gauge | Delta since last poll |
| `missed_vote_credits_5m` | Gauge | Credits missed (5 min window) |
| `missed_vote_credits_1h` | Gauge | Credits missed (1 hour window) |
| `missed_vote_credits_rate_5m` | Gauge | Miss rate per minute (5 min avg) |
| `missed_vote_credits_rate_1h` | Gauge | Miss rate per minute (1 hour avg) |
| `solana_vote_credits_efficiency_5m` | Gauge | Fraction of max credits earned (5 min) |
| `solana_vote_credits_efficiency_1h` | Gauge | Fraction of max credits earned (1 hour) |
| `solana_vote_credits_efficiency_epoch` | Gauge | Fraction of max credits earned (epoch) |
| `solana_vote_credits_per_slot_5m` | Gauge | Avg credits per slot (5 min, max 16) |
| `solana_vote_credits_per_slot_1h` | Gauge | Avg credits per slot (1 hour, max 16) |
| `solana_vote_credits_per_slot_epoch` | Gauge | Avg credits per slot (epoch, max 16) |
| `solana_vote_latency_slots_5m` | Gauge | Implied vote latency in slots (5 min) |
| `solana_vote_latency_slots_1h` | Gauge | Implied vote latency in slots (1 hour) |
| `solana_vote_latency_slots_epoch` | Gauge | Implied vote latency in slots (epoch) |
| `missed_vote_credits_total` | Counter | Cumulative missed credits |
| `rpc_up` | Gauge | RPC status (1=up, 0=down) |
| `rpc_errors` | Counter | Total RPC errors |
| `rpc_last_success` | Gauge | Last successful RPC timestamp |

### Per-Vote Histogram Metrics (WebSocket)

Real-time per-vote credit distribution via WebSocket subscription to the vote account.

| Metric | Labels | Description |
|--------|--------|-------------|
| `solana_vote_credits_histogram_count` | `window`, `credits` | Vote count per credit bucket |
| `solana_vote_credits_histogram_fraction` | `window`, `credits` | Fraction of votes per credit bucket |

**Labels:**
- `window`: `5m`, `1h`, or `epoch`
- `credits`: `0` through `16` (0 = missed, 16 = fastest)

**Example queries:**
```promql
# Votes earning 16 credits (1-slot latency) in last 5m
solana_vote_credits_histogram_count{window="5m", credits="16"}

# Fraction of votes with latency ≤ 2 slots (15-16 credits)
sum(solana_vote_credits_histogram_fraction{window="1h", credits=~"15|16"})

# Missed vote fraction this epoch
solana_vote_credits_histogram_fraction{window="epoch", credits="0"}
```

## Deployment

### Docker Compose

```yaml
services:
  tvc-tracker:
    image: ghcr.io/blocksize-capital-gmbh/tvc_tracker:latest
    container_name: tvc_tracker
    network_mode: host
    restart: unless-stopped
    command:
      - "--vote-pubkey=${VOTE_PUBKEY}"
      - "--rpc-url=${RPC_URL:-https://api.mainnet.solana.com}"
      - "--interval-secs=${INTERVAL_SECS:-60}"
```

### Prometheus Configuration

```yaml
scrape_configs:
  - job_name: "tvc-tracker"
    static_configs:
      - targets: ["localhost:7999"]
    scrape_interval: 30s
```

### Sample Alerts

```yaml
groups:
  - name: tvc-tracker
    rules:
      - alert: TVCHighMissRate5m
        expr: missed_vote_credits_rate_5m > 10
        for: 2m
        labels:
          severity: warning
        annotations:
          description: "Missing {{ $value }} credits/min over 5m avg"

      - alert: TVCMissRateIncreasing
        expr: missed_vote_credits_rate_5m > missed_vote_credits_rate_1h * 1.5
        for: 5m
        labels:
          severity: warning
        annotations:
          description: "5m miss rate 50% higher than 1h average"

      - alert: TVCRPCDown
        expr: rpc_up == 0
        for: 2m
        labels:
          severity: critical

      - alert: TVCHighMissRateEpoch
        expr: (missed_vote_credits_current_epoch / solana_vote_credits_expected_max) > 0.01
        for: 10m
        labels:
          severity: warning

      - alert: TVCHighVoteLatency
        expr: solana_vote_latency_slots_5m > 3
        for: 5m
        labels:
          severity: warning
        annotations:
          description: "Avg vote latency {{ $value }} slots (target: <2)"

      - alert: TVCLowCreditsPerSlot
        expr: solana_vote_credits_per_slot_5m < 14
        for: 5m
        labels:
          severity: warning
        annotations:
          description: "Earning {{ $value }}/16 credits per slot"

      - alert: TVCHighMissedVoteFraction
        expr: solana_vote_credits_histogram_fraction{window="5m", credits="0"} > 0.01
        for: 2m
        labels:
          severity: warning
        annotations:
          description: "{{ $value | humanizePercentage }} of votes missed in 5m"

      - alert: TVCLatencyDegraded
        expr: sum(solana_vote_credits_histogram_fraction{window="5m", credits=~"15|16"}) < 0.9
        for: 5m
        labels:
          severity: warning
        annotations:
          description: "Only {{ $value | humanizePercentage }} of votes at optimal latency"
```

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        TVC Tracker                           │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐  │
│  │  REST Poller   │  │  WS Subscriber │  │    Metrics     │  │
│  │  (aggregate)   │  │  (per-vote)    │  │    Registry    │  │
│  └───────┬────────┘  └───────┬────────┘  └───────┬────────┘  │
│          │                   │                   │           │
│  ┌───────┴───────────────────┴───────────────────┴────────┐  │
│  │            Axum HTTP Server (:7999/metrics)            │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
         │                   │                   ▲
         ▼ HTTP              ▼ WebSocket         │
   ┌───────────┐       ┌───────────┐     ┌──────┴──────┐
   │ Solana RPC│       │ Solana RPC│     │ Prometheus  │
   │  (REST)   │       │   (WS)    │     └─────────────┘
   └───────────┘       └───────────┘
```

**Data Flow:**
- **REST Poller**: Fetches aggregate epoch credits at configurable intervals
- **WS Subscriber**: Real-time vote account updates for per-vote histogram

## Development

```bash
cargo build          # Build
cargo test           # Run tests
cargo clippy         # Lint
cargo fmt            # Format
```

## Contributing

1. Fork & create a feature branch
2. Make changes with tests
3. Run `cargo fmt && cargo clippy && cargo test`
4. Submit a Pull Request

## Security

- Exporter requires read-only RPC access
- No private keys are stored or transmitted
- All metrics are public validator data

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

---

<div align="center">

## Made with ❤️ by Blocksize to support and secure the Solana ecosystem

**Validator Identity:** `HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib`

[![Stake on Blocksize](https://img.shields.io/badge/Stake_on-Blocksize-9945FF?style=for-the-badge&logo=solana&logoColor=white)](https://stakewiz.com/validator/HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib)
[![Stake on Kiwi](https://img.shields.io/badge/Stake_on-Kiwi-00C853?style=for-the-badge&logo=solana&logoColor=white)](https://kiwi.validators.app/HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib)

</div>
