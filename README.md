# Shadow-Radar 🎯

> **Stealth Insider Discovery & Copy Agent** — a real-time Solana daemon that streams live DEX trades via Yellowstone gRPC, profiles wallets through 4 strict heuristic filters, and persists high-signal targets into a SQLite database for downstream copy-trading execution.

**Read-only analytics · No private keys · No transactions**

---

## Overview

Shadow-Radar operates in two modes:

- **`live` mode** — Runs as a continuous daemon, streaming real-time trades and building rolling 24h wallet profiles. Qualified wallets are written to `targets.db` (SQLite) for your execution bot to poll.
- **`replay` mode** — One-shot historical batch scan over 12–24h of data. Outputs a ranked JSON report.

> *Which wallets on Solana are genuinely alpha — not snipers, not MEV bots, not herd-followers?*

**Supported DEXes:** PumpFun · PumpSwap · Raydium Launchpad · **Raydium AMM V4**

---

## Architecture

### Live Mode (Daemon)

Three concurrent async tasks run indefinitely:

```
Yellowstone gRPC (4 DEX programs)
        │
        ▼
  [Task 1] gRPC Streamer ──→ mpsc channel (100k buffer)
        │
        ▼
  [Task 2] Trade Processor
        ├─ InsiderCache (moka, 24h TTL)
        │     ├─ TokenRegistry   — T₀ slot tracking
        │     ├─ WalletState     — Last-20 sliding window
        │     └─ BuyIndex        — Anti-herd correlation
        │
        ├─ Heuristic Evaluator (4 strict filters)
        │
        └──→ targets.db (SQLite WAL) ←── Execution bot polls
        
  [Task 3] Maintenance (every 5 min)
        ├─ Cache eviction
        ├─ Trade pruning
        └─ Stale wallet demotion
```

### Replay Mode (Batch)

```
Yellowstone gRPC Replay → Parser → Aggregator → Filters → Ranker → alpha_wallets.json
```

### Source Modules

| File | Responsibility |
|---|---|
| `main.rs` | Mode dispatch (`live` / `replay`), daemon orchestration |
| `config.rs` | CLI args (`clap`) + `.env` loading, filter thresholds |
| `models.rs` | Core data types: `ParsedTrade`, `WalletProfile`, `DexType` |
| `grpc_streamer.rs` | Yellowstone gRPC connection (historical replay + live stream) |
| `transaction_parser.rs` | Discriminator-based parser (PumpFun/PumpSwap/Raydium) + balance-diff parser (Raydium V4) |
| `wallet_state.rs` | Per-wallet sliding window: `AnnotatedTrade`, `TokenPosition`, `CompletedCycle` |
| `token_registry.rs` | T₀ tracking with lower-slot revision resilience |
| `cache.rs` | Central moka cache orchestrator — trade ingestion, T₀ annotation, anti-herd indexing |
| `heuristics.rs` | 4 strict heuristic filters (win rate, anti-sniper, anti-whale/herd, sizing) |
| `db.rs` | SQLite persistence (WAL mode) — qualified wallets, trade log, discovery log |
| `aggregator.rs` | Batch wallet profile builder (replay mode) |
| `filters.rs` | Batch filter pipeline (replay mode) |
| `ranker.rs` | Composite score calculation and top-N selection (replay mode) |
| `output.rs` | Terminal report + JSON writer (replay mode) |

---

## Heuristic Filters (Live Mode)

Wallets must pass **all four filters** to be written to `targets.db`. All filters operate on a rolling 24-hour sliding window.

### Filter 1 — Win Rate (≥ 80%)
The wallet must have an 80%+ win rate over its last 20 completed buy→sell cycles. A win = realized PnL > 0 after fees.

### Filter 2 — Anti-Sniper (T₀ + Δt Window)
Measures the slot delta between a wallet's buy and the token's first liquidity event (T₀):

| Slot Delta | Classification | Effect |
|---|---|---|
| `< 3` | Sniper | ❌ Penalized |
| `[3, 15]` | **Insider window** | ✅ Required |
| `> 15` | Retail momentum | ❌ Penalized |

**≥ 50% of a wallet's entries must fall in the [3, 15] insider window.**

T₀ is resilient — if the daemon later observes a transaction at a lower slot, T₀ is revised downward and all affected wallets are re-annotated.

### Filter 3 — Anti-Whale & Anti-Herd
- **Anti-Whale:** Average entry size must be ≤ 5 SOL.
- **Anti-Herd:** For each buy, counts distinct wallets that buy the same token within 3 slots. If ≥ 4 copycats follow ≥ 50% of a wallet's buys → disqualified as an MEV attractor.

### Filter 4 — Sizing Profile (Sell Tranches)
Smart money scales out, not dumps. Each closed position must show:
- **≥ 2 sell tranches** (not a single 100% dump)
- **No single sell > 70%** of the total position

≥ 60% of completed positions must meet both criteria.

---

## Prerequisites

- **Rust** ≥ 1.75 (2021 edition) — [install via rustup](https://rustup.rs/)
- A **Yellowstone gRPC** endpoint with `x-token` auth (e.g. [Nexus/Kaldera](https://www.constant-k.com/), [Shyft](https://shyft.to))
- A standard **Solana RPC** endpoint (e.g. Helius, Shyft)

---

## Setup

**1. Clone and enter the directory:**
```bash
git clone <your-repo-url>
cd shadow-radar
```

**2. Configure environment:**
```bash
cp .env.example .env
```

Edit `.env` with your credentials:
```env
YELLOWSTONE_GRPC_HTTP=https://your-grpc-endpoint.com
YELLOWSTONE_GRPC_TOKEN=YOUR_GRPC_X_TOKEN
RPC_HTTP=https://your-rpc-endpoint.com
```

**3. Build (release mode):**
```bash
cargo build --release
```

---

## Usage

### Live mode (recommended — run on VPS)

Start the daemon. It runs indefinitely, streaming trades and qualifying wallets:
```bash
# Foreground (with verbose logging)
./target/release/shadow-radar --mode live --db targets.db -v

# Background (production VPS deployment)
nohup ./target/release/shadow-radar --mode live --db targets.db > shadow-radar.log 2>&1 &
```

Stops cleanly on `Ctrl+C` or `kill <pid>`.

### Replay mode (one-shot batch scan)
```bash
# 24-hour scan, top 5 wallets
./target/release/shadow-radar --mode replay

# 12-hour scan, stricter quality bar
./target/release/shadow-radar --mode replay --hours 12 --min-cycles 10 --top 3
```

### Full CLI reference
```
Usage: shadow-radar [OPTIONS]

Options:
  --mode <MODE>            Operating mode: 'live' or 'replay' [default: replay]
  --hours <HOURS>          Replay window in hours (replay mode) [default: 24]
  --db <PATH>              SQLite database path (live mode) [default: targets.db]
  --min-cycles <N>         Minimum completed trade cycles required [default: 5]
  --min-trades <N>         Minimum total trades required [default: 10]
  --top <N>                Number of top wallets to output [default: 5]
  --output <PATH>          Output JSON file path (replay mode) [default: alpha_wallets.json]
  -v, --verbose            Enable debug-level logging
  -h, --help               Print help
  -V, --version            Print version
```

---

## Output

### Live mode → `targets.db` (SQLite)

The execution bot polls this database for active targets:

```sql
SELECT address, win_rate, avg_entry_sol, composite_score
FROM qualified_wallets
WHERE is_active = 1
ORDER BY composite_score DESC;
```

**Schema:**

| Table | Purpose |
|---|---|
| `qualified_wallets` | Currently qualified wallets with metrics + `is_active` flag |
| `wallet_trades` | Recent trade history for audit/debugging |
| `discovery_log` | Timestamped log of qualification/demotion events |

The database uses `PRAGMA journal_mode=WAL` and `PRAGMA synchronous=NORMAL` for lock-free concurrent reads by the execution bot.

### Replay mode → `alpha_wallets.json`

```json
{
  "generated_at": "2026-04-15T11:00:16Z",
  "replay_window_hours": 24,
  "total_trades_scanned": 629,
  "total_wallets_seen": 398,
  "wallets_after_filters": 1,
  "top_wallets": [
    {
      "rank": 1,
      "address": "Gygj...",
      "win_rate": 0.643,
      "composite_score": 3.91,
      "dominant_dex": "PumpSwap"
    }
  ]
}
```

---

## Deployment Guide (VPS)

### Recommended timing

| Runtime | Signal quality |
|---|---|
| < 2 hours | Too early — wallets haven't completed enough cycles |
| 4–6 hours | First qualified wallets start appearing |
| **12–24 hours** | **Solid signal** — most active traders complete 20+ cycles |
| **48–72 hours** | **Best results** — patterns across Asia/US market sessions |
| **Continuous (24/7)** | **Ideal** — self-maintaining 24h rolling window |

### Resource requirements

| Resource | Requirement |
|---|---|
| CPU | 1–2 cores |
| RAM | 200–500 MB steady state |
| Disk | < 50 MB (SQLite DB) |
| Network | Stable connection to gRPC endpoint |

### Monitoring

```bash
# Check if daemon is running
pgrep -a shadow-radar

# Check latest log output
tail -20 shadow-radar.log

# Check active qualified wallets
sqlite3 targets.db "SELECT COUNT(*) FROM qualified_wallets WHERE is_active = 1;"
```

---

## Security

- **No signing, no execution.** This tool never constructs or submits a Solana transaction.
- **No private keys.** Only a gRPC auth token and an RPC URL are required.
- The `.env` file is listed in `.gitignore` and must never be committed.
- `targets.db` contains only public on-chain data (wallet addresses and trade metrics).

---

## License

Private — all rights reserved.
