// shadow-radar/src/filters.rs
//
// The four quantitative filter stages that disqualify wallets or flag
// desirable traits. Filters are applied in order A→B→C→D.
//
// Filter A: Capital Proportionality (avg trade size bounds)
// Filter B: Anti-Herd / MEV Trap (copycat buy detection)
// Filter C: Latency / Entry Delta (sniper vs momentum detection)
// Filter D: Expectancy Formula (win rate × gain/loss ratio)

use crate::config::ShadowConfig;
use crate::models::{ParsedTrade, WalletProfile};

use std::collections::HashMap;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Public: run all filters
// ---------------------------------------------------------------------------

/// Apply all four filters to the wallet profiles in-place.
///
/// `token_trades` is the per-token trade list needed for Filter B's cross-wallet analysis.
pub fn apply_all_filters(
    profiles: &mut HashMap<String, WalletProfile>,
    token_trades: &HashMap<String, Vec<ParsedTrade>>,
    config: &ShadowConfig,
) {
    let initial = profiles.len();

    filter_a_capital_proportionality(profiles, config);
    let after_a = profiles.values().filter(|p| !p.disqualified).count();

    filter_b_anti_herd_mev_trap(profiles, token_trades, config);
    let after_b = profiles.values().filter(|p| !p.disqualified).count();

    filter_c_latency_entry_delta(profiles, token_trades, config);
    let after_c = profiles.values().filter(|p| !p.disqualified).count();

    filter_d_expectancy(profiles, config);
    let after_d = profiles.values().filter(|p| !p.disqualified).count();

    info!(
        "🔬 Filter pipeline: {} → A:{} → B:{} → C:{} → D:{} wallets remaining",
        initial, after_a, after_b, after_c, after_d
    );
}

// ---------------------------------------------------------------------------
// Filter A: Capital Proportionality
// ---------------------------------------------------------------------------
// Your trade size = 0.25 SOL. Filter OUT wallets whose avg trade size
// is > 2.0 SOL (whales you can't follow) or < 0.1 SOL (dust trades).

fn filter_a_capital_proportionality(
    profiles: &mut HashMap<String, WalletProfile>,
    config: &ShadowConfig,
) {
    for profile in profiles.values_mut() {
        if profile.disqualified {
            continue;
        }

        let avg = profile.avg_trade_size_sol;

        if avg < config.min_avg_trade_sol {
            profile.disqualify(format!(
                "Filter A: avg trade size {:.4} SOL < minimum {:.1} SOL",
                avg, config.min_avg_trade_sol
            ));
            debug!(
                "❌ [A] {} — avg trade {:.4} SOL too small",
                &profile.address[..8.min(profile.address.len())],
                avg
            );
        } else if avg > config.max_avg_trade_sol {
            profile.disqualify(format!(
                "Filter A: avg trade size {:.4} SOL > maximum {:.1} SOL (whale)",
                avg, config.max_avg_trade_sol
            ));
            debug!(
                "❌ [A] {} — avg trade {:.4} SOL (whale)",
                &profile.address[..8.min(profile.address.len())],
                avg
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Filter B: Anti-Herd / MEV Trap Detection (CRITICAL)
// ---------------------------------------------------------------------------
// For each buy by wallet W on token T at slot S:
//   1. Look at ALL OTHER buys for token T in slots S and S+1
//   2. Those within tx_index > W's tx_index (same block) or next block
//      represent potential copycats (400ms–800ms window ≈ same/next slot)
//   3. If > `mev_follower_threshold` copycats consistently follow W's
//      buys (in more than `mev_herd_ratio_threshold` of W's buys),
//      disqualify W entirely.

fn filter_b_anti_herd_mev_trap(
    profiles: &mut HashMap<String, WalletProfile>,
    token_trades: &HashMap<String, Vec<ParsedTrade>>,
    config: &ShadowConfig,
) {
    // Pre-sort all token trade lists by (slot, tx_index) for binary-search-like scanning
    let mut sorted_token_trades: HashMap<String, Vec<&ParsedTrade>> = HashMap::new();
    for (mint, trades) in token_trades {
        let mut sorted: Vec<&ParsedTrade> = trades.iter().collect();
        sorted.sort_by(|a, b| a.slot.cmp(&b.slot).then(a.tx_index.cmp(&b.tx_index)));
        sorted_token_trades.insert(mint.clone(), sorted);
    }

    for profile in profiles.values_mut() {
        if profile.disqualified {
            continue;
        }

        let buys: Vec<&ParsedTrade> = profile.trades.iter().filter(|t| t.is_buy).collect();
        if buys.is_empty() {
            continue;
        }

        let mut herded_count: u32 = 0;

        for buy in &buys {
            let token_list = match sorted_token_trades.get(&buy.token_mint) {
                Some(list) => list,
                None => continue,
            };

            // Count copycat buys: same token, slot S or S+1, different wallet,
            // and positioned AFTER this wallet's transaction in block ordering
            let mut copycat_count: u32 = 0;

            for other in token_list {
                // Must be a buy from a different wallet
                if !other.is_buy || other.wallet == buy.wallet {
                    continue;
                }

                let slot_delta = other.slot.saturating_sub(buy.slot);

                if slot_delta == 0 {
                    // Same block: only count if tx_index is after ours
                    if other.tx_index > buy.tx_index {
                        copycat_count = copycat_count.saturating_add(1);
                    }
                } else if slot_delta == 1 {
                    // Next block: always counts (400ms–800ms later)
                    copycat_count = copycat_count.saturating_add(1);
                }
                // slot_delta >= 2 → outside the window, skip
            }

            if copycat_count >= config.mev_follower_threshold {
                herded_count = herded_count.saturating_add(1);
            }
        }

        // Compute herd ratio
        let herd_ratio = herded_count as f64 / buys.len() as f64;
        profile.mev_herd_ratio = herd_ratio;

        if herd_ratio >= config.mev_herd_ratio_threshold {
            profile.disqualify(format!(
                "Filter B: MEV trap — {:.0}% of buys followed by ≥{} copycats (threshold: {:.0}%)",
                herd_ratio * 100.0,
                config.mev_follower_threshold,
                config.mev_herd_ratio_threshold * 100.0
            ));
            warn!(
                "🚫 [B] {} — MEV herd ratio {:.1}%",
                &profile.address[..8.min(profile.address.len())],
                herd_ratio * 100.0
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Filter C: Latency / Entry Delta Strategy
// ---------------------------------------------------------------------------
// - "First trade" per token ≈ pool creation timestamp (approved heuristic).
// - Disqualify snipers: wallets that consistently buy within ≤2 slots of
//   pool creation (block-0 / block-1).
// - Flag momentum traders: wallets buying 5–15 minutes after pool creation.

fn filter_c_latency_entry_delta(
    profiles: &mut HashMap<String, WalletProfile>,
    token_trades: &HashMap<String, Vec<ParsedTrade>>,
    config: &ShadowConfig,
) {
    // Pre-compute first trade timestamp and slot per token (proxy for pool creation)
    let mut first_trade: HashMap<String, (u64, u64)> = HashMap::new(); // mint → (timestamp, slot)

    for (mint, trades) in token_trades {
        if let Some(earliest) = trades
            .iter()
            .filter(|t| t.timestamp > 0)
            .min_by_key(|t| (t.slot, t.tx_index))
        {
            first_trade.insert(mint.clone(), (earliest.timestamp, earliest.slot));
        }
    }

    for profile in profiles.values_mut() {
        if profile.disqualified {
            continue;
        }

        let buys: Vec<&ParsedTrade> = profile.trades.iter().filter(|t| t.is_buy).collect();
        if buys.is_empty() {
            continue;
        }

        let mut sniper_count: u32 = 0;
        let mut momentum_count: u32 = 0;

        for buy in &buys {
            let (creation_ts, creation_slot) = match first_trade.get(&buy.token_mint) {
                Some(v) => *v,
                None => continue,
            };

            // Slot-based sniper detection
            let slot_delta = buy.slot.saturating_sub(creation_slot);
            if slot_delta <= config.sniper_max_slot_delta {
                sniper_count = sniper_count.saturating_add(1);
            }

            // Timestamp-based momentum detection
            if buy.timestamp > 0 && creation_ts > 0 {
                let time_delta = buy.timestamp.saturating_sub(creation_ts);
                if time_delta >= config.momentum_entry_min_secs
                    && time_delta <= config.momentum_entry_max_secs
                {
                    momentum_count = momentum_count.saturating_add(1);
                }
            }
        }

        let sniper_ratio = sniper_count as f64 / buys.len() as f64;
        let momentum_ratio = momentum_count as f64 / buys.len() as f64;

        profile.is_sniper = sniper_ratio >= config.sniper_ratio_threshold;
        profile.momentum_trader = momentum_ratio >= config.momentum_ratio_threshold;

        if profile.is_sniper {
            profile.disqualify(format!(
                "Filter C: sniper — {:.0}% of buys within ≤{} slots of pool creation",
                sniper_ratio * 100.0,
                config.sniper_max_slot_delta
            ));
            debug!(
                "🎯 [C] {} — sniper ratio {:.1}%",
                &profile.address[..8.min(profile.address.len())],
                sniper_ratio * 100.0
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Filter D: Expectancy Formula
// ---------------------------------------------------------------------------
// Score = Win_Rate_% × (Avg_Winning_Trade_% / Avg_Losing_Trade_%)
// Minimum requirements: Win Rate ≥ 60%, Average Gain ≥ 100%.

fn filter_d_expectancy(
    profiles: &mut HashMap<String, WalletProfile>,
    config: &ShadowConfig,
) {
    for profile in profiles.values_mut() {
        if profile.disqualified {
            continue;
        }

        // Must have completed cycles to evaluate
        if profile.cycles.is_empty() {
            profile.disqualify("Filter D: no completed trade cycles".to_string());
            continue;
        }

        // Win rate check
        if profile.win_rate < config.min_win_rate {
            profile.disqualify(format!(
                "Filter D: win rate {:.1}% < minimum {:.0}%",
                profile.win_rate * 100.0,
                config.min_win_rate * 100.0
            ));
            debug!(
                "❌ [D] {} — win rate {:.1}%",
                &profile.address[..8.min(profile.address.len())],
                profile.win_rate * 100.0
            );
            continue;
        }

        // Average gain check
        if profile.avg_winning_pct < config.min_avg_gain_pct {
            profile.disqualify(format!(
                "Filter D: avg gain {:.1}% < minimum {:.0}%",
                profile.avg_winning_pct, config.min_avg_gain_pct
            ));
            debug!(
                "❌ [D] {} — avg gain {:.1}%",
                &profile.address[..8.min(profile.address.len())],
                profile.avg_winning_pct
            );
        }
    }
}
