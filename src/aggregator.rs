// shadow-radar/src/aggregator.rs
//
// Receives a stream of `ParsedTrade`s and aggregates them into
// per-wallet `WalletProfile`s with computed PnL metrics.
//
// Key design: trades are grouped by wallet, then per-token buy/sell
// sequences are paired into `TradeCycle`s for accurate win/loss tracking.

use crate::config::ShadowConfig;
use crate::models::{ParsedTrade, TradeCycle, WalletProfile};

use dashmap::DashMap;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Consume all trades from `rx`, aggregate into wallet profiles, and return
/// the complete map plus per-token trade lists (used by Filter B).
pub async fn aggregate_trades(
    mut rx: mpsc::Receiver<ParsedTrade>,
    config: &ShadowConfig,
) -> AggregationResult {
    let wallet_trades: DashMap<String, Vec<ParsedTrade>> = DashMap::new();
    let token_trades: DashMap<String, Vec<ParsedTrade>> = DashMap::new();
    let mut total_trades: u64 = 0;

    // --- Consume channel ---
    while let Some(trade) = rx.recv().await {
        total_trades = total_trades.saturating_add(1);

        // Group by wallet
        wallet_trades
            .entry(trade.wallet.clone())
            .or_default()
            .push(trade.clone());

        // Group by token mint (for Filter B cross-wallet analysis)
        token_trades
            .entry(trade.token_mint.clone())
            .or_default()
            .push(trade);
    }

    let total_wallets = wallet_trades.len() as u64;
    info!(
        "📦 Aggregation complete: {} trades across {} unique wallets, {} unique tokens",
        total_trades,
        total_wallets,
        token_trades.len()
    );

    // --- Build wallet profiles ---
    let mut profiles: HashMap<String, WalletProfile> = HashMap::new();

    for entry in wallet_trades.iter() {
        let address = entry.key().clone();
        let trades = entry.value().clone();

        if (trades.len() as u32) < config.cli.min_trades {
            debug!(
                "Skipping wallet {} — only {} trades (min: {})",
                &address[..8.min(address.len())],
                trades.len(),
                config.cli.min_trades
            );
            continue;
        }

        let profile = build_profile(address, trades, config);
        profiles.insert(profile.address.clone(), profile);
    }

    info!(
        "📊 Built {} wallet profiles (of {} total wallets — {} filtered by min-trades)",
        profiles.len(),
        total_wallets,
        total_wallets.saturating_sub(profiles.len() as u64)
    );

    // Convert token_trades DashMap to HashMap
    let token_trades_map: HashMap<String, Vec<ParsedTrade>> = token_trades
        .into_iter()
        .collect();

    AggregationResult {
        profiles,
        token_trades: token_trades_map,
        total_trades,
        total_wallets,
    }
}

// ---------------------------------------------------------------------------
// Profile builder
// ---------------------------------------------------------------------------

fn build_profile(
    address: String,
    mut trades: Vec<ParsedTrade>,
    config: &ShadowConfig,
) -> WalletProfile {
    let mut profile = WalletProfile::new(address);

    // Sort trades by (slot, tx_index) for chronological ordering
    trades.sort_by(|a, b| a.slot.cmp(&b.slot).then(a.tx_index.cmp(&b.tx_index)));

    let total_buys = trades.iter().filter(|t| t.is_buy).count() as u32;
    let total_sells = trades.iter().filter(|t| !t.is_buy).count() as u32;

    // Average trade size in SOL (across all trades)
    let total_sol: f64 = trades.iter().map(|t| t.sol_amount).sum();
    let avg_trade_size = if !trades.is_empty() {
        total_sol / trades.len() as f64
    } else {
        0.0
    };

    // --- Match buy→sell cycles per token ---
    let cycles = match_trade_cycles(&trades);

    // --- Compute PnL metrics ---
    let (win_rate, avg_winning_pct, avg_losing_pct, expectancy) =
        compute_pnl_metrics(&cycles);

    profile.trades = trades;
    profile.cycles = cycles;
    profile.total_buys = total_buys;
    profile.total_sells = total_sells;
    profile.avg_trade_size_sol = avg_trade_size;
    profile.win_rate = win_rate;
    profile.avg_winning_pct = avg_winning_pct;
    profile.avg_losing_pct = avg_losing_pct;
    profile.expectancy_score = expectancy;

    // Min cycles check
    if profile.cycles.len() < config.cli.min_cycles as usize {
        profile.disqualify(format!(
            "Insufficient completed cycles: {} (min: {})",
            profile.cycles.len(),
            config.cli.min_cycles
        ));
    }

    profile
}

// ---------------------------------------------------------------------------
// Trade-cycle matching (FIFO per token)
// ---------------------------------------------------------------------------

/// For each token, pair buys with subsequent sells in FIFO order.
/// A "cycle" is one buy followed by one sell of the same token.
fn match_trade_cycles(trades: &[ParsedTrade]) -> Vec<TradeCycle> {
    let mut cycles = Vec::new();

    // Group by token
    let mut per_token: HashMap<String, Vec<&ParsedTrade>> = HashMap::new();
    for trade in trades {
        per_token
            .entry(trade.token_mint.clone())
            .or_default()
            .push(trade);
    }

    for token_trades in per_token.values() {
        let mut pending_buys: Vec<&ParsedTrade> = Vec::new();

        for trade in token_trades {
            if trade.is_buy {
                pending_buys.push(trade);
            } else if let Some(buy) = pending_buys.first() {
                // Match this sell to the oldest pending buy (FIFO)
                let buy_sol = buy.sol_amount;
                let sell_sol = trade.sol_amount;

                // PnL = (sell - buy) / buy × 100
                let pnl_pct = if buy_sol > 0.0 {
                    ((sell_sol - buy_sol) / buy_sol) * 100.0
                } else {
                    0.0
                };

                cycles.push(TradeCycle {
                    token_mint: trade.token_mint.clone(),
                    buy_sol,
                    sell_sol,
                    buy_price: buy.price,
                    sell_price: trade.price,
                    buy_slot: buy.slot,
                    sell_slot: trade.slot,
                    buy_timestamp: buy.timestamp,
                    sell_timestamp: trade.timestamp,
                    pnl_pct,
                    is_win: pnl_pct > 0.0,
                });

                pending_buys.remove(0);
            }
            // If no pending buy, ignore this sell (incomplete cycle)
        }
    }

    cycles
}

// ---------------------------------------------------------------------------
// PnL metric computation
// ---------------------------------------------------------------------------

/// Returns (win_rate, avg_winning_pct, avg_losing_pct, expectancy).
fn compute_pnl_metrics(cycles: &[TradeCycle]) -> (f64, f64, f64, f64) {
    if cycles.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let wins: Vec<&TradeCycle> = cycles.iter().filter(|c| c.is_win).collect();
    let losses: Vec<&TradeCycle> = cycles.iter().filter(|c| !c.is_win).collect();

    let win_rate = wins.len() as f64 / cycles.len() as f64;

    let avg_winning_pct = if !wins.is_empty() {
        wins.iter().map(|c| c.pnl_pct).sum::<f64>() / wins.len() as f64
    } else {
        0.0
    };

    let avg_losing_pct = if !losses.is_empty() {
        // Average loss is negative; we take absolute value for the formula
        losses.iter().map(|c| c.pnl_pct.abs()).sum::<f64>() / losses.len() as f64
    } else {
        1.0 // Avoid division by zero: if no losses, treat avg_loss as minimal
    };

    // Expectancy = Win_Rate × (Avg_Win / Avg_Loss)
    let expectancy = if avg_losing_pct > 0.0 {
        win_rate * (avg_winning_pct / avg_losing_pct)
    } else {
        // No losses → infinite expectancy, cap it
        win_rate * avg_winning_pct
    };

    (win_rate, avg_winning_pct, avg_losing_pct, expectancy)
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

pub struct AggregationResult {
    pub profiles: HashMap<String, WalletProfile>,
    /// All trades grouped by token mint (for Filter B cross-wallet analysis)
    pub token_trades: HashMap<String, Vec<ParsedTrade>>,
    pub total_trades: u64,
    pub total_wallets: u64,
}
