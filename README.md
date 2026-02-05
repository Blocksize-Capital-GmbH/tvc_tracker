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

| Metric | Type | Description |
|--------|------|-------------|
| `solana_vote_credits_expected_max` | Gauge | Max theoretical credits (slots × 16) |
| `solana_vote_credits_actual` | Gauge | Actual credits earned this epoch |
| `missed_vote_credits_current_epoch` | Gauge | Credits missed this epoch |
| `missed_vote_credits_last_epoch` | Gauge | Credits missed last epoch |
| `missed_vote_credits_since_last_poll` | Gauge | Delta since last poll |
| `missed_vote_credits_5m` | Gauge | Credits missed (5 min window) |
| `missed_vote_credits_1h` | Gauge | Credits missed (1 hour window) |
| `missed_vote_credits_total` | Counter | Cumulative missed credits |
| `rpc_up` | Gauge | RPC status (1=up, 0=down) |
| `rpc_errors` | Counter | Total RPC errors |
| `rpc_last_success` | Gauge | Last successful RPC timestamp |

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
      - alert: TVCHighMissedCredits5m
        expr: missed_vote_credits_5m > 100
        for: 2m
        labels:
          severity: warning

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
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      TVC Tracker                        │
│  ┌──────────┐  ┌──────────┐  ┌───────────────────────┐  │
│  │   RPC    │  │ Metrics  │  │   Poller (interval)   │  │
│  │  Client  │  │ Registry │  │                       │  │
│  └────┬─────┘  └────┬─────┘  └───────────┬───────────┘  │
│       │             └────────────────────┤              │
│       │                                  │              │
│  ┌────┴──────────────────────────────────┴───────────┐  │
│  │         Axum HTTP Server (:7999/metrics)          │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
         │                              ▲
         ▼                              │
   ┌───────────┐                ┌───────┴───────┐
   │ Solana RPC│                │  Prometheus   │
   └───────────┘                └───────────────┘
```

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

For security issues, please contact: security@blocksize-capital.com

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

---

<div align="center">

## Made with ❤️ by Blocksize to support and secure the Solana ecosystem

**Validator Identity:** `HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib`

[![Stake on Blocksize](https://img.shields.io/badge/Stake_on-Blocksize-9945FF?style=for-the-badge&logo=solana&logoColor=white)](https://stakewiz.com/validator/HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib)
[![Stake on Kiwi](https://img.shields.io/badge/Stake_on-Kiwi-00C853?style=for-the-badge&logo=solana&logoColor=white)](https://kiwi.validators.app/HMk1qny4fvMnajErxjXG5kT89JKV4cx1PKa9zhQBF9ib)

</div>
