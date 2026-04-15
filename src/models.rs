// shadow-radar/src/models.rs
//
// Core data structures for the Shadow-Radar wallet profiler.
// All monetary values are in SOL (f64) or lamports (u64) as annotated.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// DEX type enum (mirrors sniper-bot but standalone)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DexType {
    PumpFun,
    PumpSwap,
    RaydiumLaunchpad,
    Unknown,
}

impl fmt::Display for DexType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DexType::PumpFun => write!(f, "PumpFun"),
            DexType::PumpSwap => write!(f, "PumpSwap"),
            DexType::RaydiumLaunchpad => write!(f, "RaydiumLaunchpad"),
            DexType::Unknown => write!(f, "Unknown"),
        }
    }
}

// ---------------------------------------------------------------------------
// Individual parsed trade
// ---------------------------------------------------------------------------

/// A single DEX swap extracted from a confirmed transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParsedTrade {
    /// Wallet address (signer / fee payer)
    pub wallet: String,
    /// Token mint address
    pub token_mint: String,
    /// true = buy (SOL → token), false = sell (token → SOL)
    pub is_buy: bool,
    /// Absolute SOL amount moved (always positive)
    pub sol_amount: f64,
    /// Absolute token amount moved (always positive)
    pub token_amount: f64,
    /// Price in lamports-per-token (scaled ×1e9)
    pub price: u64,
    /// Slot in which the transaction was confirmed
    pub slot: u64,
    /// On-chain unix timestamp (seconds)
    pub timestamp: u64,
    /// Which DEX processed this swap
    pub dex_type: DexType,
    /// Transaction signature (base58)
    pub signature: String,
    /// Index of this transaction within its block (for intra-block ordering)
    pub tx_index: u32,
}

// ---------------------------------------------------------------------------
// Per-token trade pair for PnL calculation
// ---------------------------------------------------------------------------

/// Matched buy→sell cycle for a single token by one wallet.
#[derive(Clone, Debug)]
pub struct TradeCycle {
    pub token_mint: String,
    pub buy_sol: f64,
    pub sell_sol: f64,
    pub buy_price: u64,
    pub sell_price: u64,
    pub buy_slot: u64,
    pub sell_slot: u64,
    pub buy_timestamp: u64,
    pub sell_timestamp: u64,
    /// Profit/loss percentage: ((sell - buy) / buy) × 100
    pub pnl_pct: f64,
    pub is_win: bool,
}

// ---------------------------------------------------------------------------
// Wallet profile (intermediate, pre-filter)
// ---------------------------------------------------------------------------

/// Aggregated profile for a single wallet across all observed trades.
#[derive(Clone, Debug)]
pub struct WalletProfile {
    pub address: String,
    /// All individual trades by this wallet
    pub trades: Vec<ParsedTrade>,
    /// Matched buy→sell cycles
    pub cycles: Vec<TradeCycle>,

    // --- Computed metrics ---
    pub total_buys: u32,
    pub total_sells: u32,
    pub avg_trade_size_sol: f64,

    // PnL metrics (from completed cycles only)
    pub win_rate: f64,
    pub avg_winning_pct: f64,
    pub avg_losing_pct: f64,
    pub expectancy_score: f64,

    // Filter flags
    pub is_sniper: bool,
    pub momentum_trader: bool,
    /// Fraction of buys that are followed by ≥ mev_threshold copycats
    pub mev_herd_ratio: f64,

    // Disqualification
    pub disqualified: bool,
    pub disqualification_reasons: Vec<String>,
}

impl WalletProfile {
    pub fn new(address: String) -> Self {
        Self {
            address,
            trades: Vec::new(),
            cycles: Vec::new(),
            total_buys: 0,
            total_sells: 0,
            avg_trade_size_sol: 0.0,
            win_rate: 0.0,
            avg_winning_pct: 0.0,
            avg_losing_pct: 0.0,
            expectancy_score: 0.0,
            is_sniper: false,
            momentum_trader: false,
            mev_herd_ratio: 0.0,
            disqualified: false,
            disqualification_reasons: Vec::new(),
        }
    }

    /// Mark this wallet as disqualified with a reason.
    pub fn disqualify(&mut self, reason: String) {
        self.disqualified = true;
        self.disqualification_reasons.push(reason);
    }
}

// ---------------------------------------------------------------------------
// Final output: Alpha wallet
// ---------------------------------------------------------------------------

/// A wallet that passed all four filters — safe for copy-trading consideration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlphaWallet {
    pub rank: u32,
    pub address: String,
    pub win_rate: f64,
    pub avg_trade_size_sol: f64,
    pub expectancy_score: f64,
    pub total_trades: u32,
    pub completed_cycles: u32,
    pub avg_gain_pct: f64,
    pub avg_loss_pct: f64,
    pub momentum_score: f64,
    pub mev_safety_score: f64,
    pub composite_score: f64,
    pub dominant_dex: String,
}

/// Top-level output structure for `alpha_wallets.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlphaWalletReport {
    pub generated_at: String,
    pub replay_window_hours: u64,
    pub total_trades_scanned: u64,
    pub total_wallets_seen: u64,
    pub wallets_after_filters: u64,
    pub top_wallets: Vec<AlphaWallet>,
}
