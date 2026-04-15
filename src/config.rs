// shadow-radar/src/config.rs
//
// Configuration loading from CLI args + .env file.
// All filter thresholds are centralized here for easy tuning.

use anyhow::{Context, Result};
use clap::Parser;
use std::env;

// ---------------------------------------------------------------------------
// CLI Arguments
// ---------------------------------------------------------------------------

/// Shadow-Radar: Advanced Solana Wallet Profiler
///
/// Mines 12-24h of historical DEX swap data via Yellowstone gRPC replay,
/// profiles wallets with quantitative filters, and ranks the safest
/// copy-trading targets.
#[derive(Parser, Debug, Clone)]
#[command(name = "shadow-radar", version, about, long_about = None)]
pub struct CliArgs {
    /// Replay window in hours (12 or 24)
    #[arg(long, default_value_t = 24)]
    pub hours: u64,

    /// Minimum number of completed trade cycles to consider a wallet
    #[arg(long, default_value_t = 5)]
    pub min_cycles: u32,

    /// Minimum total trades (buys + sells) to consider a wallet
    #[arg(long, default_value_t = 10)]
    pub min_trades: u32,

    /// Number of top wallets to output
    #[arg(long, default_value_t = 5)]
    pub top: usize,

    /// Output file path
    #[arg(long, default_value = "alpha_wallets.json")]
    pub output: String,

    /// Verbose logging (debug-level)
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,
}

// ---------------------------------------------------------------------------
// Shadow-Radar Configuration
// ---------------------------------------------------------------------------

/// All tuneable parameters for the Shadow-Radar pipeline.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    // --- RPC / gRPC ---
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub rpc_http: String,

    // --- CLI ---
    pub cli: CliArgs,

    // --- Filter A: Capital Proportionality ---
    /// Minimum average trade size in SOL to pass
    pub min_avg_trade_sol: f64,
    /// Maximum average trade size in SOL to pass
    pub max_avg_trade_sol: f64,

    // --- Filter B: Anti-Herd / MEV Trap ---
    /// Number of copycat buys that constitute a "herd"
    pub mev_follower_threshold: u32,
    /// Fraction of a wallet's buys that must show herding to disqualify (0.0-1.0)
    pub mev_herd_ratio_threshold: f64,

    // --- Filter C: Latency / Entry Delta ---
    /// Minimum seconds after pool creation for momentum entry (5 minutes)
    pub momentum_entry_min_secs: u64,
    /// Maximum seconds after pool creation for momentum entry (15 minutes)
    pub momentum_entry_max_secs: u64,
    /// Fraction of buys in the momentum window to flag as momentum trader
    pub momentum_ratio_threshold: f64,
    /// Maximum slots after first trade to be considered a "sniper" (block-0 / block-1)
    pub sniper_max_slot_delta: u64,
    /// Fraction of buys at sniper distance to flag as sniper
    pub sniper_ratio_threshold: f64,

    // --- Filter D: Expectancy ---
    /// Minimum win rate to pass (0.0-1.0)
    pub min_win_rate: f64,
    /// Minimum average gain percentage on winning trades
    pub min_avg_gain_pct: f64,
}

impl ShadowConfig {
    /// Load configuration from CLI args and `.env` file.
    pub fn load(cli: CliArgs) -> Result<Self> {
        dotenv::dotenv().ok();

        let yellowstone_grpc_http = env::var("YELLOWSTONE_GRPC_HTTP")
            .context("YELLOWSTONE_GRPC_HTTP not set in environment or .env")?;
        let yellowstone_grpc_token = env::var("YELLOWSTONE_GRPC_TOKEN")
            .context("YELLOWSTONE_GRPC_TOKEN not set in environment or .env")?;
        let rpc_http = env::var("RPC_HTTP")
            .context("RPC_HTTP not set in environment or .env")?;

        Ok(Self {
            yellowstone_grpc_http,
            yellowstone_grpc_token,
            rpc_http,
            cli,

            // Filter A defaults — match user's 0.25 SOL trade size ±tolerance
            min_avg_trade_sol: 0.1,
            max_avg_trade_sol: 2.0,

            // Filter B defaults
            mev_follower_threshold: 3,
            mev_herd_ratio_threshold: 0.50,

            // Filter C defaults — 5 to 15 minutes
            momentum_entry_min_secs: 300,
            momentum_entry_max_secs: 900,
            momentum_ratio_threshold: 0.60,
            sniper_max_slot_delta: 2,
            sniper_ratio_threshold: 0.50,

            // Filter D defaults
            min_win_rate: 0.60,
            min_avg_gain_pct: 100.0,
        })
    }
}
