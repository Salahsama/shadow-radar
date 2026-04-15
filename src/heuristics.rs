// shadow-radar/src/heuristics.rs
//
// The four strict heuristic filters for the live insider discovery engine.
// Each filter evaluates a WalletState snapshot and returns a FilterVerdict.
//
// These are PURE FUNCTIONS — no I/O, no cache mutation. The cache orchestrator
// calls them after each trade ingestion to determine wallet qualification.
//
// Filter 1: Win Rate           — 80%+ over last 20 completed cycles
// Filter 2: Anti-Sniper (T₀+Δt) — ≥50% of entries in [3, 15] slot window
// Filter 3: Anti-Whale/Anti-Herd — avg entry ≤ 5 SOL + copycat detection
// Filter 4: Sizing Profile     — sells in tranches, no single >70% dump

use crate::cache::InsiderCache;
use crate::wallet_state::WalletState;

use tracing::{debug, trace};

// ---------------------------------------------------------------------------
// FilterVerdict — tri-state result from each filter
// ---------------------------------------------------------------------------

/// Result of evaluating a single heuristic filter against a wallet.
#[derive(Clone, Debug)]
pub enum FilterVerdict {
    /// Wallet passes this filter.
    Pass,
    /// Wallet fails this filter — reason provided.
    Fail(String),
    /// Insufficient data to evaluate — re-evaluate later.
    /// This is NOT a pass. The wallet stays in "pending" status.
    Insufficient(String),
}

impl FilterVerdict {
    #[inline]
    pub fn is_pass(&self) -> bool {
        matches!(self, FilterVerdict::Pass)
    }

    #[inline]
    pub fn is_fail(&self) -> bool {
        matches!(self, FilterVerdict::Fail(_))
    }
}

// ---------------------------------------------------------------------------
// HeuristicConfig — tuneable filter thresholds
// ---------------------------------------------------------------------------

/// All tuneable parameters for the 4 heuristic filters.
#[derive(Clone, Debug)]
pub struct HeuristicConfig {
    // --- Filter 1: Win Rate ---
    /// Minimum win rate over the last 20 cycles. Default: 0.80 (80%).
    pub min_win_rate: f64,
    /// Minimum completed cycles to evaluate win rate. Default: 5.
    pub min_cycles_for_win_rate: usize,

    // --- Filter 2: Anti-Sniper ---
    /// Minimum slot delta from T₀ for the insider window. Default: 3.
    pub anti_sniper_min_slot_delta: u64,
    /// Maximum slot delta from T₀ for the insider window. Default: 15.
    pub anti_sniper_max_slot_delta: u64,
    /// Minimum fraction of initial entries that must fall in [min, max].
    /// Per quant strategist: ≥50%. Default: 0.50.
    pub min_insider_window_ratio: f64,
    /// Minimum annotated buys (with T₀ data) to evaluate. Default: 5.
    pub min_annotated_buys: usize,

    // --- Filter 3: Anti-Whale / Anti-Herd ---
    /// Maximum average entry size in SOL. Default: 5.0.
    pub max_avg_entry_sol: f64,
    /// Copycat detection: slot window around a buy to look for copycats. Default: 3.
    pub copycat_cluster_window_slots: u64,
    /// Minimum copycats within the window to flag a "herded" buy. Default: 4.
    pub copycat_min_correlated_buys: u32,
    /// Fraction of a wallet's buys that must show herding to disqualify. Default: 0.50.
    pub herd_ratio_threshold: f64,
    /// Minimum buys to evaluate anti-herd. Default: 5.
    pub min_buys_for_herd: usize,

    // --- Filter 4: Sizing Profile ---
    /// Minimum sell tranches per position to qualify. Default: 2.
    pub min_sell_tranches: u32,
    /// Maximum fraction of a position sold in a single transaction. Default: 0.70.
    pub max_single_sell_pct: f64,
    /// Minimum fraction of closed positions that meet the tranche criteria. Default: 0.60.
    pub min_tranche_compliance_ratio: f64,
    /// Minimum closed positions to evaluate. Default: 3.
    pub min_positions_for_sizing: usize,
}

impl Default for HeuristicConfig {
    fn default() -> Self {
        Self {
            // Filter 1
            min_win_rate: 0.80,
            min_cycles_for_win_rate: 5,

            // Filter 2
            anti_sniper_min_slot_delta: 3,
            anti_sniper_max_slot_delta: 15,
            min_insider_window_ratio: 0.50,
            min_annotated_buys: 5,

            // Filter 3
            max_avg_entry_sol: 5.0,
            copycat_cluster_window_slots: 3,
            copycat_min_correlated_buys: 4,
            herd_ratio_threshold: 0.50,
            min_buys_for_herd: 5,

            // Filter 4
            min_sell_tranches: 2,
            max_single_sell_pct: 0.70,
            min_tranche_compliance_ratio: 0.60,
            min_positions_for_sizing: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// FullEvaluation — aggregate result of all 4 filters
// ---------------------------------------------------------------------------

/// Aggregate result of evaluating all 4 heuristic filters.
#[derive(Clone, Debug)]
pub struct FullEvaluation {
    pub win_rate: FilterVerdict,
    pub anti_sniper: FilterVerdict,
    pub anti_whale: FilterVerdict,
    pub anti_herd: FilterVerdict,
    pub sizing: FilterVerdict,

    /// True only if ALL 5 sub-verdicts are Pass.
    pub qualified: bool,
    /// Human-readable reasons for any failures.
    pub failure_reasons: Vec<String>,
}

impl FullEvaluation {
    /// Build a FullEvaluation from the 5 sub-verdicts.
    fn from_verdicts(
        win_rate: FilterVerdict,
        anti_sniper: FilterVerdict,
        anti_whale: FilterVerdict,
        anti_herd: FilterVerdict,
        sizing: FilterVerdict,
    ) -> Self {
        let all_pass = win_rate.is_pass()
            && anti_sniper.is_pass()
            && anti_whale.is_pass()
            && anti_herd.is_pass()
            && sizing.is_pass();

        let mut reasons = Vec::new();
        for (name, verdict) in [
            ("WinRate", &win_rate),
            ("AntiSniper", &anti_sniper),
            ("AntiWhale", &anti_whale),
            ("AntiHerd", &anti_herd),
            ("Sizing", &sizing),
        ] {
            match verdict {
                FilterVerdict::Fail(reason) => {
                    reasons.push(format!("[{}] {}", name, reason));
                }
                FilterVerdict::Insufficient(reason) => {
                    reasons.push(format!("[{}] INSUFFICIENT: {}", name, reason));
                }
                FilterVerdict::Pass => {}
            }
        }

        Self {
            win_rate,
            anti_sniper,
            anti_whale,
            anti_herd,
            sizing,
            qualified: all_pass,
            failure_reasons: reasons,
        }
    }
}

// ---------------------------------------------------------------------------
// Filter 1: Win Rate (over last 20 completed cycles)
// ---------------------------------------------------------------------------

/// Evaluate whether the wallet has an 80%+ win rate over its last 20 trades.
/// Win = realized PnL > 0 after the cycle is closed.
pub fn filter_win_rate(state: &WalletState, config: &HeuristicConfig) -> FilterVerdict {
    let cycle_count = state.completed_cycles.len();

    if cycle_count < config.min_cycles_for_win_rate {
        return FilterVerdict::Insufficient(format!(
            "Only {} cycles (need ≥{})",
            cycle_count, config.min_cycles_for_win_rate
        ));
    }

    let wr = state.win_rate_last_20;

    if wr >= config.min_win_rate {
        trace!(
            "✅ F1 WinRate: {:.1}% ≥ {:.0}%",
            wr * 100.0,
            config.min_win_rate * 100.0
        );
        FilterVerdict::Pass
    } else {
        FilterVerdict::Fail(format!(
            "Win rate {:.1}% < {:.0}%",
            wr * 100.0,
            config.min_win_rate * 100.0
        ))
    }
}

// ---------------------------------------------------------------------------
// Filter 2: Anti-Sniper (T₀ + Δt window enforcement)
// ---------------------------------------------------------------------------
//
// Quant strategist correction (strict):
//   - Buys with slot delta < 3      → SNIPER (bad)
//   - Buys with delta in [3, 15]    → INSIDER WINDOW (good)
//   - Buys with delta > 15          → RETAIL MOMENTUM (bad — NOT neutral)
//
// A wallet must have ≥50% of its initial token entries fall STRICTLY within
// the [3, 15] slot window. Anything outside is penalized.

pub fn filter_anti_sniper(state: &WalletState, config: &HeuristicConfig) -> FilterVerdict {
    // Count only buys that have T₀ annotation data
    let annotated_buys: Vec<_> = state
        .recent_trades
        .iter()
        .filter(|t| t.inner.is_buy && t.t0_was_known)
        .collect();

    if annotated_buys.len() < config.min_annotated_buys {
        return FilterVerdict::Insufficient(format!(
            "Only {} annotated buys (need ≥{})",
            annotated_buys.len(),
            config.min_annotated_buys
        ));
    }

    let total = annotated_buys.len() as f64;

    // Count each category
    let sniper_count = annotated_buys.iter().filter(|t| t.is_sniper).count();
    let insider_count = annotated_buys
        .iter()
        .filter(|t| t.is_in_insider_window)
        .count();
    let retail_count = annotated_buys.iter().filter(|t| t.is_retail).count();

    let insider_ratio = insider_count as f64 / total;

    // Log the breakdown for diagnostics
    trace!(
        "F2 AntiSniper: {}/{} insider ({:.0}%), {}/{} sniper, {}/{} retail",
        insider_count,
        annotated_buys.len(),
        insider_ratio * 100.0,
        sniper_count,
        annotated_buys.len(),
        retail_count,
        annotated_buys.len()
    );

    // Pass ONLY if ≥50% of entries are in the insider window
    if insider_ratio >= config.min_insider_window_ratio {
        FilterVerdict::Pass
    } else {
        FilterVerdict::Fail(format!(
            "Insider window ratio {:.1}% < {:.0}% ({} sniper, {} insider, {} retail out of {})",
            insider_ratio * 100.0,
            config.min_insider_window_ratio * 100.0,
            sniper_count,
            insider_count,
            retail_count,
            annotated_buys.len()
        ))
    }
}

// ---------------------------------------------------------------------------
// Filter 3a: Anti-Whale (avg entry size)
// ---------------------------------------------------------------------------

/// Reject wallets whose average entry size exceeds the whale threshold.
pub fn filter_anti_whale(state: &WalletState, config: &HeuristicConfig) -> FilterVerdict {
    let buy_count = state
        .recent_trades
        .iter()
        .filter(|t| t.inner.is_buy)
        .count();

    if buy_count < 3 {
        return FilterVerdict::Insufficient(format!(
            "Only {} buys (need ≥3 for avg entry evaluation)",
            buy_count
        ));
    }

    let avg = state.avg_entry_size_sol;

    if avg <= config.max_avg_entry_sol {
        trace!("✅ F3a AntiWhale: avg entry {:.3} SOL ≤ {:.1} SOL", avg, config.max_avg_entry_sol);
        FilterVerdict::Pass
    } else {
        FilterVerdict::Fail(format!(
            "Avg entry {:.3} SOL > {:.1} SOL max",
            avg, config.max_avg_entry_sol
        ))
    }
}

// ---------------------------------------------------------------------------
// Filter 3b: Anti-Herd (copycat buy cluster detection)
// ---------------------------------------------------------------------------
//
// For each buy by wallet W on token T at slot S:
//   Count distinct wallets that also buy T within [S, S + window_slots].
//   If ≥ copycat_min_correlated_buys copycats, this buy is "herded".
// If ≥ herd_ratio_threshold of W's buys are herded → disqualify.
//
// This requires access to the InsiderCache for cross-wallet lookups.

pub async fn filter_anti_herd(
    state: &WalletState,
    cache: &InsiderCache,
    config: &HeuristicConfig,
) -> FilterVerdict {
    let buys: Vec<_> = state
        .recent_trades
        .iter()
        .filter(|t| t.inner.is_buy)
        .collect();

    if buys.len() < config.min_buys_for_herd {
        return FilterVerdict::Insufficient(format!(
            "Only {} buys (need ≥{} for herd evaluation)",
            buys.len(),
            config.min_buys_for_herd
        ));
    }

    let mut herded_count: u32 = 0;

    for buy in &buys {
        let copycats = cache
            .count_copycats(
                &buy.inner.token_mint,
                &buy.inner.wallet,
                buy.inner.slot,
                buy.inner.tx_index,
                config.copycat_cluster_window_slots,
            )
            .await;

        if copycats >= config.copycat_min_correlated_buys {
            herded_count = herded_count.saturating_add(1);
        }
    }

    let herd_ratio = herded_count as f64 / buys.len() as f64;

    if herd_ratio < config.herd_ratio_threshold {
        trace!(
            "✅ F3b AntiHerd: herd ratio {:.1}% < {:.0}%",
            herd_ratio * 100.0,
            config.herd_ratio_threshold * 100.0
        );
        FilterVerdict::Pass
    } else {
        FilterVerdict::Fail(format!(
            "Herd ratio {:.1}% ≥ {:.0}% ({} of {} buys followed by ≥{} copycats)",
            herd_ratio * 100.0,
            config.herd_ratio_threshold * 100.0,
            herded_count,
            buys.len(),
            config.copycat_min_correlated_buys
        ))
    }
}

// ---------------------------------------------------------------------------
// Filter 4: Sizing Profile (sell-in-tranches detection)
// ---------------------------------------------------------------------------
//
// A sophisticated wallet scales out (sells in 2+ tranches) rather than
// dumping 100% of their holdings at once.
//
// For each completed cycle:
//   - sell_tranche_count >= min_sell_tranches (default: 2)
//   - max_single_sell_pct <= max_single_sell_pct threshold (default: 0.70)
// Wallet passes if ≥60% of its positions meet BOTH criteria.

pub fn filter_sizing(state: &WalletState, config: &HeuristicConfig) -> FilterVerdict {
    let cycles: Vec<_> = state.completed_cycles.iter().collect();

    if cycles.len() < config.min_positions_for_sizing {
        return FilterVerdict::Insufficient(format!(
            "Only {} completed positions (need ≥{} for sizing evaluation)",
            cycles.len(),
            config.min_positions_for_sizing
        ));
    }

    let mut compliant_count: usize = 0;

    for cycle in &cycles {
        let meets_tranche_count = cycle.sell_tranche_count >= config.min_sell_tranches;
        let meets_max_pct = cycle.max_single_sell_pct <= config.max_single_sell_pct;

        if meets_tranche_count && meets_max_pct {
            compliant_count += 1;
        }
    }

    let compliance_ratio = compliant_count as f64 / cycles.len() as f64;

    if compliance_ratio >= config.min_tranche_compliance_ratio {
        trace!(
            "✅ F4 Sizing: {:.1}% compliant ≥ {:.0}%",
            compliance_ratio * 100.0,
            config.min_tranche_compliance_ratio * 100.0
        );
        FilterVerdict::Pass
    } else {
        FilterVerdict::Fail(format!(
            "Tranche compliance {:.1}% < {:.0}% ({}/{} positions with ≥{} tranches and max {:.0}% single sell)",
            compliance_ratio * 100.0,
            config.min_tranche_compliance_ratio * 100.0,
            compliant_count,
            cycles.len(),
            config.min_sell_tranches,
            config.max_single_sell_pct * 100.0
        ))
    }
}

// ---------------------------------------------------------------------------
// Full evaluation — run all 4 filters (5 sub-verdicts)
// ---------------------------------------------------------------------------

/// Run all 4 heuristic filters against a wallet state snapshot.
///
/// Note: `filter_anti_herd` requires cache access and is async.
pub async fn evaluate_wallet(
    state: &WalletState,
    cache: &InsiderCache,
    config: &HeuristicConfig,
) -> FullEvaluation {
    let f1 = filter_win_rate(state, config);
    let f2 = filter_anti_sniper(state, config);
    let f3a = filter_anti_whale(state, config);
    let f3b = filter_anti_herd(state, cache, config).await;
    let f4 = filter_sizing(state, config);

    let eval = FullEvaluation::from_verdicts(f1, f2, f3a, f3b, f4);

    if eval.qualified {
        debug!(
            "🎯 QUALIFIED: {} — win rate {:.1}%, insider ratio {:.1}%, avg entry {:.3} SOL",
            &state.address[..8.min(state.address.len())],
            state.win_rate_last_20 * 100.0,
            state.insider_window_ratio * 100.0,
            state.avg_entry_size_sol
        );
    }

    eval
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DexType, ParsedTrade};
    use crate::wallet_state::{AnnotatedTrade, CompletedCycle, WalletState};
    use std::collections::VecDeque;

    fn make_annotated_buy(
        mint: &str,
        sol: f64,
        slot: u64,
        t0_slot: Option<u64>,
    ) -> AnnotatedTrade {
        let trade = ParsedTrade {
            wallet: "test_wallet".to_string(),
            token_mint: mint.to_string(),
            is_buy: true,
            sol_amount: sol,
            token_amount: sol * 1000.0,
            price: 1_000_000,
            slot,
            timestamp: 1700000000,
            dex_type: DexType::PumpSwap,
            signature: format!("sig_{}", slot),
            tx_index: 0,
        };
        AnnotatedTrade::annotate(trade, t0_slot, 3, 15)
    }

    fn make_winning_cycle(tranches: u32, max_sell_pct: f64) -> CompletedCycle {
        CompletedCycle {
            token_mint: "test_token".to_string(),
            buy_sol: 1.0,
            sell_sol: 1.5,
            pnl_pct: 50.0,
            is_win: true,
            sell_tranche_count: tranches,
            max_single_sell_pct: max_sell_pct,
            closed_at: 1700000000,
            entry_slot_delta: Some(10),
        }
    }

    fn make_losing_cycle(tranches: u32, max_sell_pct: f64) -> CompletedCycle {
        CompletedCycle {
            token_mint: "test_token".to_string(),
            buy_sol: 1.0,
            sell_sol: 0.5,
            pnl_pct: -50.0,
            is_win: false,
            sell_tranche_count: tranches,
            max_single_sell_pct: max_sell_pct,
            closed_at: 1700000000,
            entry_slot_delta: Some(8),
        }
    }

    // --- Filter 1: Win Rate tests ---

    #[test]
    fn test_win_rate_pass() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 17 wins + 3 losses = 85% win rate (passes 80%)
        for _ in 0..17 {
            state
                .completed_cycles
                .push_back(make_winning_cycle(2, 0.5));
        }
        for _ in 0..3 {
            state
                .completed_cycles
                .push_back(make_losing_cycle(2, 0.5));
        }
        state.win_rate_last_20 = 17.0 / 20.0;

        assert!(filter_win_rate(&state, &config).is_pass());
    }

    #[test]
    fn test_win_rate_fail() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 14 wins + 6 losses = 70% (fails 80%)
        for _ in 0..14 {
            state
                .completed_cycles
                .push_back(make_winning_cycle(2, 0.5));
        }
        for _ in 0..6 {
            state
                .completed_cycles
                .push_back(make_losing_cycle(2, 0.5));
        }
        state.win_rate_last_20 = 14.0 / 20.0;

        assert!(filter_win_rate(&state, &config).is_fail());
    }

    #[test]
    fn test_win_rate_insufficient() {
        let config = HeuristicConfig::default();
        let state = WalletState::new("test".to_string());
        // 0 cycles → insufficient
        assert!(matches!(
            filter_win_rate(&state, &config),
            FilterVerdict::Insufficient(_)
        ));
    }

    // --- Filter 2: Anti-Sniper tests ---

    #[test]
    fn test_anti_sniper_pass_insider_window() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 6 buys in insider window [3, 15], 2 sniper, 2 retail
        // 6/10 = 60% ≥ 50%
        for i in 0..6 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 105 + i, Some(100)));
        }
        for i in 0..2 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 101 + i, Some(100)));
        }
        for i in 0..2 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 120 + i, Some(100)));
        }

        assert!(filter_anti_sniper(&state, &config).is_pass());
    }

    #[test]
    fn test_anti_sniper_fail_too_many_snipers() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 6 snipers (delta 0-2), 2 insiders, 2 retail → only 20% insider
        for i in 0..6 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 100 + i % 3, Some(100)));
        }
        for i in 0..2 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 105 + i, Some(100)));
        }
        for i in 0..2 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 120 + i, Some(100)));
        }

        assert!(filter_anti_sniper(&state, &config).is_fail());
    }

    #[test]
    fn test_anti_sniper_fail_mostly_retail() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 7 retail (delta > 15), 1 insider, 2 sniper → 10% insider
        for i in 0..7 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 200 + i, Some(100)));
        }
        state
            .recent_trades
            .push_back(make_annotated_buy("token", 1.0, 110, Some(100)));
        for i in 0..2 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 1.0, 100 + i, Some(100)));
        }

        assert!(filter_anti_sniper(&state, &config).is_fail());
    }

    // --- Filter 3a: Anti-Whale tests ---

    #[test]
    fn test_anti_whale_pass() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());
        state.avg_entry_size_sol = 2.5;
        // Need at least 3 buys
        for i in 0..5 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 2.5, 100 + i, None));
        }
        assert!(filter_anti_whale(&state, &config).is_pass());
    }

    #[test]
    fn test_anti_whale_fail() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());
        state.avg_entry_size_sol = 7.5;
        for i in 0..5 {
            state
                .recent_trades
                .push_back(make_annotated_buy("token", 7.5, 100 + i, None));
        }
        assert!(filter_anti_whale(&state, &config).is_fail());
    }

    // --- Filter 4: Sizing tests ---

    #[test]
    fn test_sizing_pass_multi_tranche() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 4 positions with 2+ tranches and max 50% single sell → 100% compliant
        for _ in 0..4 {
            state
                .completed_cycles
                .push_back(make_winning_cycle(3, 0.40));
        }

        assert!(filter_sizing(&state, &config).is_pass());
    }

    #[test]
    fn test_sizing_fail_single_dump() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 4 positions: all with 1 tranche (100% dump) → 0% compliant
        for _ in 0..4 {
            state.completed_cycles.push_back(CompletedCycle {
                token_mint: "test".to_string(),
                buy_sol: 1.0,
                sell_sol: 1.5,
                pnl_pct: 50.0,
                is_win: true,
                sell_tranche_count: 1,    // single dump
                max_single_sell_pct: 1.0, // 100% in one go
                closed_at: 1700000000,
                entry_slot_delta: Some(10),
            });
        }

        assert!(filter_sizing(&state, &config).is_fail());
    }

    #[test]
    fn test_sizing_fail_large_single_sell() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 4 positions: 2 tranches but one sell is 80% (> 70% threshold)
        for _ in 0..4 {
            state
                .completed_cycles
                .push_back(make_winning_cycle(2, 0.80));
        }

        assert!(filter_sizing(&state, &config).is_fail());
    }

    #[test]
    fn test_sizing_mixed_compliance() {
        let config = HeuristicConfig::default();
        let mut state = WalletState::new("test".to_string());

        // 3 compliant (tranche=3, max_sell=40%) + 2 non-compliant (tranche=1, dump=100%)
        // 3/5 = 60% → passes at 60% threshold
        for _ in 0..3 {
            state
                .completed_cycles
                .push_back(make_winning_cycle(3, 0.40));
        }
        for _ in 0..2 {
            state.completed_cycles.push_back(CompletedCycle {
                token_mint: "test".to_string(),
                buy_sol: 1.0,
                sell_sol: 1.5,
                pnl_pct: 50.0,
                is_win: true,
                sell_tranche_count: 1,
                max_single_sell_pct: 1.0,
                closed_at: 1700000000,
                entry_slot_delta: Some(10),
            });
        }

        assert!(filter_sizing(&state, &config).is_pass());
    }
}
