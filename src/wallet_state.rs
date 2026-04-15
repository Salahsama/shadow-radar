// shadow-radar/src/wallet_state.rs
//
// Core in-memory state for continuous wallet behavior tracking.
// Designed for a 24-hour sliding window with O(1) metric updates
// on every incoming trade.
//
// Key design decisions:
//   - VecDeque for recent_trades: O(1) push_back / pop_front (circular buffer)
//   - Pre-computed metrics updated incrementally (no full re-scan per trade)
//   - TokenPosition tracks open positions with sell-tranche granularity
//   - CompletedCycle is evicted after 24h via the parent moka cache TTL

use crate::models::ParsedTrade;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

/// Maximum recent trades to retain per wallet for win-rate evaluation.
/// Quant spec: "80%+ win rate over its last 20 memecoin trades."
pub const RECENT_TRADES_WINDOW: usize = 20;

/// Maximum completed cycles to retain in the rolling window.
/// Beyond this, oldest cycles are evicted on push (FIFO).
pub const MAX_COMPLETED_CYCLES: usize = 200;

// ---------------------------------------------------------------------------
// AnnotatedTrade — a ParsedTrade enriched with T₀ delta data
// ---------------------------------------------------------------------------

/// A trade enriched with slot-delta information relative to the token's
/// first observed liquidity event (T₀).
///
/// The anti-sniper filter operates on `slot_delta_from_t0`:
///   - delta < 3 slots     → SNIPER (reject)
///   - delta in [3, 15]    → INSIDER WINDOW (accept)
///   - delta > 15 slots    → RETAIL MOMENTUM (not neutral — penalize)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnotatedTrade {
    /// The underlying parsed trade from the gRPC stream.
    pub inner: ParsedTrade,

    /// Slot delta from this token's T₀ (first liquidity init).
    /// `None` if the token registry has no T₀ record yet.
    pub slot_delta_from_t0: Option<u64>,

    /// True if this buy happened in the same slot as T₀ (delta = 0)
    /// or within the bot-sniper window (delta < min_slot_delta).
    pub is_sniper: bool,

    /// True if this buy falls within the insider window [min, max] slot delta.
    pub is_in_insider_window: bool,

    /// True if this buy is a retail momentum entry (delta > max_slot_delta).
    pub is_retail: bool,

    /// Whether T₀ was known at the time of annotation.
    /// If false, this trade should be re-evaluated when T₀ is discovered.
    pub t0_was_known: bool,
}

impl AnnotatedTrade {
    /// Create an annotated trade from a raw parsed trade.
    ///
    /// # Parameters
    /// - `trade`: The raw parsed trade from the gRPC parser.
    /// - `t0_slot`: The T₀ slot for this token (if known).
    /// - `min_slot_delta`: Minimum slot delta for the insider window (default: 3).
    /// - `max_slot_delta`: Maximum slot delta for the insider window (default: 15).
    pub fn annotate(
        trade: ParsedTrade,
        t0_slot: Option<u64>,
        min_slot_delta: u64,
        max_slot_delta: u64,
    ) -> Self {
        let (slot_delta, sniper, in_window, is_ret, t0_known) = match t0_slot {
            Some(t0) => {
                let delta = trade.slot.saturating_sub(t0);
                (
                    Some(delta),
                    delta < min_slot_delta,
                    delta >= min_slot_delta && delta <= max_slot_delta,
                    delta > max_slot_delta,
                    true,
                )
            }
            None => (None, false, false, false, false),
        };

        Self {
            inner: trade,
            slot_delta_from_t0: slot_delta,
            is_sniper: sniper,
            is_in_insider_window: in_window,
            is_retail: is_ret,
            t0_was_known: t0_known,
        }
    }

    /// Re-annotate this trade with an updated T₀ value.
    /// Called when TokenRegistry detects a lower T₀ slot.
    pub fn re_annotate(
        &mut self,
        new_t0_slot: u64,
        min_slot_delta: u64,
        max_slot_delta: u64,
    ) {
        let delta = self.inner.slot.saturating_sub(new_t0_slot);
        self.slot_delta_from_t0 = Some(delta);
        self.is_sniper = delta < min_slot_delta;
        self.is_in_insider_window = delta >= min_slot_delta && delta <= max_slot_delta;
        self.is_retail = delta > max_slot_delta;
        self.t0_was_known = true;
    }
}

// ---------------------------------------------------------------------------
// SellEvent — individual sell tranche for sizing analysis
// ---------------------------------------------------------------------------

/// A single sell event within a token position, used to calculate
/// whether the wallet "scales out" (sells in tranches) vs dumps 100%.
#[derive(Clone, Debug)]
pub struct SellEvent {
    /// SOL returned in this sell.
    pub sol_amount: f64,
    /// Fraction of the total position sold in this single event.
    /// Calculated as: `sol_amount / total_bought_sol` at time of sell.
    pub pct_of_position: f64,
    /// Slot of the sell transaction.
    pub slot: u64,
    /// Unix timestamp.
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// TokenPosition — open or partially-closed position on a single token
// ---------------------------------------------------------------------------

/// Tracks a wallet's open (or partially closed) position in a single token.
/// Used for sell-tranche analysis (Filter 4: Sizing Profile).
#[derive(Clone, Debug)]
pub struct TokenPosition {
    /// Token mint address.
    pub mint: String,
    /// Cumulative SOL spent buying this token.
    pub total_bought_sol: f64,
    /// Cumulative SOL received selling this token.
    pub total_sold_sol: f64,
    /// Number of buy transactions.
    pub buy_count: u32,
    /// Number of sell transactions.
    pub sell_count: u32,
    /// Slot of the first buy (for ordering).
    pub first_buy_slot: u64,
    /// Timestamp of the first buy.
    pub first_buy_timestamp: u64,
    /// All sell events for tranche analysis.
    pub sells: Vec<SellEvent>,
    /// Whether this position has been fully closed and accounted for.
    pub closed: bool,
}

impl TokenPosition {
    pub fn new(mint: String, first_buy_sol: f64, first_buy_slot: u64, first_buy_ts: u64) -> Self {
        Self {
            mint,
            total_bought_sol: first_buy_sol,
            total_sold_sol: 0.0,
            buy_count: 1,
            sell_count: 0,
            first_buy_slot,
            first_buy_timestamp: first_buy_ts,
            sells: Vec::new(),
            closed: false,
        }
    }

    /// Record a buy event for this token.
    pub fn add_buy(&mut self, sol_amount: f64) {
        self.total_bought_sol += sol_amount;
        self.buy_count = self.buy_count.saturating_add(1);
    }

    /// Record a sell event for this token.
    /// Returns `true` if the position is now fully closed (sold >= bought).
    pub fn add_sell(&mut self, sol_amount: f64, slot: u64, timestamp: u64) -> bool {
        let pct = if self.total_bought_sol > 0.0 {
            sol_amount / self.total_bought_sol
        } else {
            1.0
        };

        self.sells.push(SellEvent {
            sol_amount,
            pct_of_position: pct,
            slot,
            timestamp,
        });

        self.total_sold_sol += sol_amount;
        self.sell_count = self.sell_count.saturating_add(1);

        // Position is "closed" when total sells >= 90% of buys
        // (allowing for slippage/fees eating into the exact amount)
        let close_threshold = self.total_bought_sol * 0.90;
        if self.total_sold_sol >= close_threshold {
            self.closed = true;
        }

        self.closed
    }

    /// Returns the largest single sell as a fraction of the total position.
    /// Used for the "no-dump" sizing filter.
    pub fn max_single_sell_pct(&self) -> f64 {
        self.sells
            .iter()
            .map(|s| s.pct_of_position)
            .fold(0.0_f64, f64::max)
    }

    /// Returns the PnL percentage for this position (if closed).
    pub fn pnl_pct(&self) -> f64 {
        if self.total_bought_sol > 0.0 {
            ((self.total_sold_sol - self.total_bought_sol) / self.total_bought_sol) * 100.0
        } else {
            0.0
        }
    }
}

// ---------------------------------------------------------------------------
// CompletedCycle — a fully closed buy→sell(s) cycle
// ---------------------------------------------------------------------------

/// A completed trade cycle: one token, bought and fully (or mostly) sold.
/// Stored in the rolling 24h window for win-rate calculation.
#[derive(Clone, Debug)]
pub struct CompletedCycle {
    /// Token mint address.
    pub token_mint: String,
    /// Total SOL spent buying.
    pub buy_sol: f64,
    /// Total SOL received selling.
    pub sell_sol: f64,
    /// PnL as a percentage: ((sell - buy) / buy) × 100.
    pub pnl_pct: f64,
    /// Did this cycle realize positive PnL?
    pub is_win: bool,
    /// Number of sell tranches used to close this position.
    pub sell_tranche_count: u32,
    /// Maximum single sell as fraction of total position.
    pub max_single_sell_pct: f64,
    /// Unix timestamp when this cycle was closed (for 24h eviction).
    pub closed_at: u64,
    /// Slot delta of the initial buy from T₀.
    pub entry_slot_delta: Option<u64>,
}

// ---------------------------------------------------------------------------
// WalletState — the main per-wallet behavior tracker
// ---------------------------------------------------------------------------

/// Complete in-memory state for a single wallet, maintained as a
/// sliding window over the last 24 hours of activity.
///
/// All metrics are pre-computed and updated incrementally on each
/// incoming trade — no full re-scan required.
#[derive(Clone, Debug)]
pub struct WalletState {
    /// Wallet base58 address.
    pub address: String,

    /// Last N annotated trades (circular buffer, FIFO eviction).
    pub recent_trades: VecDeque<AnnotatedTrade>,

    /// Open (and recently closed) positions per token.
    pub per_token_positions: HashMap<String, TokenPosition>,

    /// Completed buy→sell cycles in the rolling 24h window.
    pub completed_cycles: VecDeque<CompletedCycle>,

    /// Unix timestamp of the last trade processed for this wallet.
    pub last_updated: u64,

    // -----------------------------------------------------------------------
    // Pre-computed metrics (updated on every trade)
    // -----------------------------------------------------------------------

    /// Win rate over the last `RECENT_TRADES_WINDOW` completed cycles.
    /// Value in [0.0, 1.0].
    pub win_rate_last_20: f64,

    /// Average SOL size of all buy entries in the recent window.
    pub avg_entry_size_sol: f64,

    /// Average number of sell tranches per closed position.
    pub avg_sell_tranches: f64,

    /// Maximum fraction of a position sold in a single transaction
    /// (averaged across all closed positions).
    pub avg_max_single_sell_pct: f64,

    /// Fraction of initial buy entries that fall in the insider window [3, 15] slots.
    pub insider_window_ratio: f64,

    /// Fraction of initial buy entries that are sniper buys (< 3 slots from T₀).
    pub sniper_ratio: f64,

    /// Fraction of initial buy entries that are retail (> 15 slots from T₀).
    pub retail_ratio: f64,

    /// Whether this wallet currently passes all 4 heuristic filters.
    pub is_qualified: bool,

    /// Reasons for disqualification (if any).
    pub disqualification_reasons: Vec<String>,

    /// Total trades processed (lifetime, including evicted).
    pub lifetime_trade_count: u64,
}

impl WalletState {
    /// Create a new empty wallet state.
    pub fn new(address: String) -> Self {
        Self {
            address,
            recent_trades: VecDeque::with_capacity(RECENT_TRADES_WINDOW + 1),
            per_token_positions: HashMap::new(),
            completed_cycles: VecDeque::with_capacity(MAX_COMPLETED_CYCLES + 1),
            last_updated: 0,
            win_rate_last_20: 0.0,
            avg_entry_size_sol: 0.0,
            avg_sell_tranches: 0.0,
            avg_max_single_sell_pct: 1.0,
            insider_window_ratio: 0.0,
            sniper_ratio: 0.0,
            retail_ratio: 0.0,
            is_qualified: false,
            disqualification_reasons: Vec::new(),
            lifetime_trade_count: 0,
        }
    }

    /// Ingest a new annotated trade and update all metrics.
    ///
    /// This is the hot path — called for every trade in the live stream.
    /// All metric updates are O(1) amortized (no full re-scan).
    pub fn ingest_trade(&mut self, trade: AnnotatedTrade) {
        self.last_updated = trade.inner.timestamp;
        self.lifetime_trade_count = self.lifetime_trade_count.saturating_add(1);

        let is_buy = trade.inner.is_buy;
        let sol_amount = trade.inner.sol_amount;
        let token_mint = trade.inner.token_mint.clone();
        let slot = trade.inner.slot;
        let timestamp = trade.inner.timestamp;

        // --- Push into recent trades (circular buffer) ---
        self.recent_trades.push_back(trade);
        if self.recent_trades.len() > RECENT_TRADES_WINDOW {
            self.recent_trades.pop_front();
        }

        // --- Update per-token position ---
        if is_buy {
            let position = self.per_token_positions
                .entry(token_mint.clone())
                .or_insert_with(|| TokenPosition::new(
                    token_mint.clone(),
                    0.0,
                    slot,
                    timestamp,
                ));
            position.add_buy(sol_amount);
        } else {
            // Sell
            if let Some(position) = self.per_token_positions.get_mut(&token_mint) {
                let closed = position.add_sell(sol_amount, slot, timestamp);
                if closed {
                    // Extract completed cycle
                    let cycle = CompletedCycle {
                        token_mint: token_mint.clone(),
                        buy_sol: position.total_bought_sol,
                        sell_sol: position.total_sold_sol,
                        pnl_pct: position.pnl_pct(),
                        is_win: position.pnl_pct() > 0.0,
                        sell_tranche_count: position.sell_count,
                        max_single_sell_pct: position.max_single_sell_pct(),
                        closed_at: timestamp,
                        entry_slot_delta: None, // Will be set by cache orchestrator
                    };

                    self.completed_cycles.push_back(cycle);
                    if self.completed_cycles.len() > MAX_COMPLETED_CYCLES {
                        self.completed_cycles.pop_front();
                    }

                    // Remove closed position from tracking
                    self.per_token_positions.remove(&token_mint);
                }
            }
            // If no matching position, this is an orphan sell — ignore
        }

        // --- Recompute metrics ---
        self.recompute_metrics();
    }

    /// Re-annotate all recent trades for a specific token when T₀ changes.
    /// Called by the cache orchestrator when TokenRegistry detects a lower T₀.
    pub fn re_annotate_token(
        &mut self,
        token_mint: &str,
        new_t0_slot: u64,
        min_slot_delta: u64,
        max_slot_delta: u64,
    ) {
        for trade in self.recent_trades.iter_mut() {
            if trade.inner.token_mint == token_mint {
                trade.re_annotate(new_t0_slot, min_slot_delta, max_slot_delta);
            }
        }
        // Recompute after re-annotation
        self.recompute_metrics();
    }

    /// Full metric recomputation from current state.
    /// Called after every trade ingestion. Designed to be fast:
    /// iterates only `recent_trades` (max 20 items) and `completed_cycles`.
    fn recompute_metrics(&mut self) {
        // --- Win rate over last 20 completed cycles ---
        let recent_cycles: Vec<&CompletedCycle> = self.completed_cycles
            .iter()
            .rev()
            .take(RECENT_TRADES_WINDOW)
            .collect();

        if !recent_cycles.is_empty() {
            let wins = recent_cycles.iter().filter(|c| c.is_win).count();
            self.win_rate_last_20 = wins as f64 / recent_cycles.len() as f64;
        } else {
            self.win_rate_last_20 = 0.0;
        }

        // --- Average entry size (buy trades only in recent window) ---
        let buy_trades: Vec<&AnnotatedTrade> = self.recent_trades
            .iter()
            .filter(|t| t.inner.is_buy)
            .collect();

        if !buy_trades.is_empty() {
            let total_sol: f64 = buy_trades.iter().map(|t| t.inner.sol_amount).sum();
            self.avg_entry_size_sol = total_sol / buy_trades.len() as f64;
        } else {
            self.avg_entry_size_sol = 0.0;
        }

        // --- Sell tranche metrics (from completed cycles) ---
        if !recent_cycles.is_empty() {
            let total_tranches: f64 = recent_cycles
                .iter()
                .map(|c| c.sell_tranche_count as f64)
                .sum();
            self.avg_sell_tranches = total_tranches / recent_cycles.len() as f64;

            let total_max_sell_pct: f64 = recent_cycles
                .iter()
                .map(|c| c.max_single_sell_pct)
                .sum();
            self.avg_max_single_sell_pct = total_max_sell_pct / recent_cycles.len() as f64;
        }

        // --- Slot delta ratios (only first buys per token) ---
        // We use recent_trades and look at buys that have T₀ data.
        let annotated_buys: Vec<&AnnotatedTrade> = self.recent_trades
            .iter()
            .filter(|t| t.inner.is_buy && t.t0_was_known)
            .collect();

        if !annotated_buys.is_empty() {
            let total = annotated_buys.len() as f64;
            let snipers = annotated_buys.iter().filter(|t| t.is_sniper).count() as f64;
            let insiders = annotated_buys.iter().filter(|t| t.is_in_insider_window).count() as f64;
            let retail = annotated_buys.iter().filter(|t| t.is_retail).count() as f64;

            self.sniper_ratio = snipers / total;
            self.insider_window_ratio = insiders / total;
            self.retail_ratio = retail / total;
        }
    }

    /// Evict completed cycles older than the given cutoff timestamp.
    /// Called periodically by the cache orchestrator.
    pub fn evict_stale_cycles(&mut self, cutoff_timestamp: u64) {
        while let Some(front) = self.completed_cycles.front() {
            if front.closed_at < cutoff_timestamp {
                self.completed_cycles.pop_front();
            } else {
                break; // Cycles are in chronological order
            }
        }
        // Recompute after eviction
        self.recompute_metrics();
    }

    /// Returns true if this wallet has enough data for meaningful evaluation.
    pub fn has_sufficient_data(&self) -> bool {
        // Need at least 5 completed cycles and 10 total recent trades
        self.completed_cycles.len() >= 5 && self.recent_trades.len() >= 10
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DexType, ParsedTrade};

    fn make_buy_trade(wallet: &str, mint: &str, sol: f64, slot: u64, ts: u64) -> ParsedTrade {
        ParsedTrade {
            wallet: wallet.to_string(),
            token_mint: mint.to_string(),
            is_buy: true,
            sol_amount: sol,
            token_amount: sol * 1000.0,
            price: 1_000_000,
            slot,
            timestamp: ts,
            dex_type: DexType::PumpSwap,
            signature: format!("sig_{}", slot),
            tx_index: 0,
        }
    }

    fn make_sell_trade(wallet: &str, mint: &str, sol: f64, slot: u64, ts: u64) -> ParsedTrade {
        ParsedTrade {
            wallet: wallet.to_string(),
            token_mint: mint.to_string(),
            is_buy: false,
            sol_amount: sol,
            token_amount: sol * 1000.0,
            price: 1_500_000,
            slot,
            timestamp: ts,
            dex_type: DexType::PumpSwap,
            signature: format!("sig_{}", slot),
            tx_index: 0,
        }
    }

    #[test]
    fn test_annotated_trade_sniper_detection() {
        let trade = make_buy_trade("wallet1", "token1", 1.0, 102, 1000);
        let annotated = AnnotatedTrade::annotate(trade, Some(100), 3, 15);

        assert_eq!(annotated.slot_delta_from_t0, Some(2));
        assert!(annotated.is_sniper);
        assert!(!annotated.is_in_insider_window);
        assert!(!annotated.is_retail);
    }

    #[test]
    fn test_annotated_trade_insider_window() {
        let trade = make_buy_trade("wallet1", "token1", 1.0, 110, 1000);
        let annotated = AnnotatedTrade::annotate(trade, Some(100), 3, 15);

        assert_eq!(annotated.slot_delta_from_t0, Some(10));
        assert!(!annotated.is_sniper);
        assert!(annotated.is_in_insider_window);
        assert!(!annotated.is_retail);
    }

    #[test]
    fn test_annotated_trade_retail() {
        let trade = make_buy_trade("wallet1", "token1", 1.0, 120, 1000);
        let annotated = AnnotatedTrade::annotate(trade, Some(100), 3, 15);

        assert_eq!(annotated.slot_delta_from_t0, Some(20));
        assert!(!annotated.is_sniper);
        assert!(!annotated.is_in_insider_window);
        assert!(annotated.is_retail);
    }

    #[test]
    fn test_annotated_trade_unknown_t0() {
        let trade = make_buy_trade("wallet1", "token1", 1.0, 110, 1000);
        let annotated = AnnotatedTrade::annotate(trade, None, 3, 15);

        assert!(annotated.slot_delta_from_t0.is_none());
        assert!(!annotated.is_sniper);
        assert!(!annotated.is_in_insider_window);
        assert!(!annotated.t0_was_known);
    }

    #[test]
    fn test_token_position_sell_tranches() {
        let mut pos = TokenPosition::new("mint1".to_string(), 2.0, 100, 1000);

        // Sell in 3 tranches
        assert!(!pos.add_sell(0.5, 110, 1010)); // 25%
        assert!(!pos.add_sell(0.7, 120, 1020)); // 35%
        assert!(pos.add_sell(0.8, 130, 1030));  // 40% → total 100% → closed

        assert_eq!(pos.sell_count, 3);
        assert!(pos.closed);
        assert!((pos.max_single_sell_pct() - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_wallet_state_buy_sell_cycle() {
        let mut state = WalletState::new("testWallet".to_string());

        // Buy 2 SOL of token1
        let buy = AnnotatedTrade::annotate(
            make_buy_trade("testWallet", "token1", 2.0, 100, 1000),
            Some(95),
            3,
            15,
        );
        state.ingest_trade(buy);
        assert_eq!(state.per_token_positions.len(), 1);

        // Sell 1 SOL (tranche 1)
        let sell1 = AnnotatedTrade::annotate(
            make_sell_trade("testWallet", "token1", 1.0, 110, 1010),
            Some(95),
            3,
            15,
        );
        state.ingest_trade(sell1);
        assert_eq!(state.per_token_positions.len(), 1); // still open

        // Sell 1.5 SOL (tranche 2 — closes position)
        let sell2 = AnnotatedTrade::annotate(
            make_sell_trade("testWallet", "token1", 1.5, 120, 1020),
            Some(95),
            3,
            15,
        );
        state.ingest_trade(sell2);
        assert_eq!(state.per_token_positions.len(), 0); // closed
        assert_eq!(state.completed_cycles.len(), 1);

        let cycle = &state.completed_cycles[0];
        assert!(cycle.is_win); // sold 2.5 SOL for 2.0 SOL cost
        assert_eq!(cycle.sell_tranche_count, 2);
    }

    #[test]
    fn test_re_annotate_on_t0_change() {
        let trade = make_buy_trade("wallet1", "token1", 1.0, 110, 1000);
        let mut annotated = AnnotatedTrade::annotate(trade, Some(100), 3, 15);

        // Original: delta = 10, insider window
        assert!(annotated.is_in_insider_window);

        // T₀ updated to slot 108 (lower slot observed)
        annotated.re_annotate(108, 3, 15);
        // New delta = 2 → sniper
        assert_eq!(annotated.slot_delta_from_t0, Some(2));
        assert!(annotated.is_sniper);
        assert!(!annotated.is_in_insider_window);
    }
}
