# Shadow-Radar 🎯

> **Advanced Solana Smart Money Profiler** — mines historical DEX swap data via Yellowstone gRPC replay, profiles wallets through a 4-stage quantitative filter pipeline, and ranks the safest copy-trading targets.

**Read-only analytics · No private keys · No transactions**

---

## Overview

Shadow-Radar is a high-performance Rust CLI tool built to answer one question:

> *Which wallets on Solana are genuinely alpha — not snipers, not MEV bots, not herd-followers?*

It replays 12–24 hours of live DEX swap data from a Yellowstone gRPC stream, builds per-wallet trade profiles, and runs them through four mathematically rigorous filters. The surviving wallets are ranked by a composite Expectancy Score and written to a JSON report ready to feed into your copy-trading workflow.

**Supported DEXes:** PumpFun · PumpSwap · Raydium

---

## Architecture

The pipeline runs in 5 sequential phases:

```
Yellowstone gRPC Replay
        │
        ▼
 [Phase 1] grpc_streamer   — Streams raw transactions from historical slots
        │
        ▼
 [Phase 2] transaction_parser + aggregator
                           — Parses DEX swap instructions, aggregates into wallet profiles
        │
        ▼
 [Phase 3] filters         — Applies 4-stage filter pipeline (A → B → C → D)
        │
        ▼
 [Phase 4] ranker          — Computes composite score, selects top N wallets
        │
        ▼
 [Phase 5] output          — Prints terminal report + writes alpha_wallets.json
```

### Source Modules

| File | Responsibility |
|---|---|
| `main.rs` | CLI entry point, pipeline orchestration |
| `config.rs` | CLI args (`clap`) + `.env` loading, all filter thresholds |
| `models.rs` | Core data types: `ParsedTrade`, `WalletProfile`, `TradeCycle` |
| `grpc_streamer.rs` | Yellowstone gRPC connection, slot replay, raw tx streaming |
| `transaction_parser.rs` | DEX discriminator-based instruction parser (PumpFun/PumpSwap/Raydium) |
| `aggregator.rs` | Builds wallet profiles from the trade stream via async mpsc channel |
| `filters.rs` | 4-stage quantitative filter pipeline |
| `ranker.rs` | Composite score calculation and top-N selection |
| `output.rs` | Terminal report (colored) + JSON file writer |

---

## Filter Pipeline

Wallets are disqualified in order. Only wallets that survive all four filters are ranked.

### Filter A — Capital Proportionality
Removes wallets whose average trade size is outside the band `[0.1, 2.0] SOL`.
- Dust traders (`< 0.1 SOL`) are noise.
- Whales (`> 2.0 SOL`) move markets and can't be safely followed at retail scale.

### Filter B — Anti-Herd / MEV Trap Detection
For each buy by wallet W on token T at slot S, counts the number of **copycat buys** from other wallets within the same slot (later tx index) or the next slot (≤ 800ms window).

If `≥ 3 copycats` follow `≥ 50%` of a wallet's buys → **disqualified as an MEV attractor or pump orchestrator**.

This is the most critical filter — it separates genuine alpha from wallets that are simply being front-run or followed by bots.

### Filter C — Latency / Entry Delta
Detects two patterns based on the time delta between pool creation and first entry:

| Pattern | Criteria | Action |
|---|---|---|
| **Sniper** | Buys within ≤ 2 slots of pool creation in ≥ 50% of trades | ❌ Disqualified |
| **Momentum Trader** | Buys 5–15 minutes after pool creation in ≥ 60% of trades | ✅ Flagged (preferred) |

Snipers rely on block-0 execution advantages that are non-replicable and introduce extreme risk.

### Filter D — Expectancy Formula
Enforces minimum statistical edge:

```
Expectancy Score = Win_Rate × (Avg_Gain% / Avg_Loss%)
```

**Minimum requirements to pass:**
- Win Rate ≥ **60%**
- Average Gain on winning trades ≥ **100%**

---

## Prerequisites

- **Rust** ≥ 1.75 (2021 edition) — [install via rustup](https://rustup.rs/)
- A **Yellowstone gRPC** endpoint with historical slot replay support (e.g. [Shyft](https://shyft.to))
- A standard **Solana RPC** endpoint for slot-time lookups (e.g. Helius, Shyft)

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
YELLOWSTONE_GRPC_HTTP=https://grpc.ny.shyft.to
YELLOWSTONE_GRPC_TOKEN=YOUR_GRPC_TOKEN
RPC_HTTP=https://rpc.shyft.to?api_key=YOUR_API_KEY
```

**3. Build (release mode):**
```bash
cargo build --release
```

The binary will be at `./target/release/shadow-radar`.

---

## Usage

### Quick start (24-hour scan, top 5 wallets)
```bash
./target/release/shadow-radar
```

### Custom scan window
```bash
# 12-hour scan
./target/release/shadow-radar --hours 12

# Stricter quality bar: 10 completed cycles, top 3 wallets
./target/release/shadow-radar --hours 24 --min-cycles 10 --top 3
```

### Full CLI reference
```
Usage: shadow-radar [OPTIONS]

Options:
  --hours <HOURS>          Replay window in hours [default: 24]
  --min-cycles <N>         Minimum completed trade cycles required [default: 5]
  --min-trades <N>         Minimum total trades (buys + sells) required [default: 10]
  --top <N>                Number of top wallets to output [default: 5]
  --output <PATH>          Output JSON file path [default: alpha_wallets.json]
  -v, --verbose            Enable debug-level logging
  -h, --help               Print help
  -V, --version            Print version
```

---

## Output

### Terminal report
A color-coded table is printed to stdout on completion, showing each wallet's key metrics.

### JSON report (`alpha_wallets.json`)
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
      "address": "Gygj9QQby4j2jryqyqBHvLP7ctv2SaANgh4sCb69BUpA",
      "win_rate": 0.643,
      "avg_trade_size_sol": 0.1998,
      "expectancy_score": 4.03,
      "total_trades": 54,
      "completed_cycles": 14,
      "avg_gain_pct": 209.01,
      "avg_loss_pct": 33.38,
      "momentum_score": 1.0,
      "mev_safety_score": 0.769,
      "composite_score": 3.91,
      "dominant_dex": "PumpSwap"
    }
  ]
}
```

### Score fields explained

| Field | Description |
|---|---|
| `expectancy_score` | `win_rate × (avg_gain / avg_loss)` — core edge metric |
| `momentum_score` | Fraction of buys in the 5–15 min momentum window (higher = better) |
| `mev_safety_score` | `1 - mev_herd_ratio` — how often the wallet trades without attracting copycats |
| `composite_score` | Weighted combination used for final ranking |
| `dominant_dex` | DEX responsible for the most trades by this wallet |

---

## Security

- **No signing, no execution.** This tool never constructs or submits a Solana transaction.
- **No private keys.** Only a gRPC auth token and an RPC URL are required.
- The `.env` file is listed in `.gitignore` and must never be committed.

---

## License

Private — all rights reserved.
