// shadow-radar/src/ranker.rs
//
// Scoring and ranking engine.
// Takes all wallet profiles that passed the filter pipeline, computes a
// composite score (expectancy × momentum_bonus × mev_safety), and returns
// the top N as AlphaWallet structs.

use crate::config::ShadowConfig;
use crate::models::{AlphaWallet, DexType, WalletProfile};

use std::collections::HashMap;
use tracing::info;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Rank all non-disqualified wallet profiles and return the top N.
pub fn rank_wallets(
    profiles: &HashMap<String, WalletProfile>,
    config: &ShadowConfig,
) -> Vec<AlphaWallet> {
    let mut candidates: Vec<(&str, f64, &WalletProfile)> = profiles
        .values()
        .filter(|p| !p.disqualified)
        .map(|p| {
            let score = compute_composite_score(p);
            (p.address.as_str(), score, p)
        })
        .collect();

    // Sort descending by composite score
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top N
    let top: Vec<AlphaWallet> = candidates
        .into_iter()
        .take(config.cli.top)
        .enumerate()
        .map(|(i, (_, composite_score, profile))| {
            let dominant_dex = find_dominant_dex(profile);
            let momentum_score = if profile.momentum_trader { 1.2 } else { 1.0 };
            let mev_safety = 1.0 - profile.mev_herd_ratio;

            AlphaWallet {
                rank: (i + 1) as u32,
                address: profile.address.clone(),
                win_rate: profile.win_rate,
                avg_trade_size_sol: profile.avg_trade_size_sol,
                expectancy_score: profile.expectancy_score,
                total_trades: profile.total_buys.saturating_add(profile.total_sells),
                completed_cycles: profile.cycles.len() as u32,
                avg_gain_pct: profile.avg_winning_pct,
                avg_loss_pct: profile.avg_losing_pct,
                momentum_score,
                mev_safety_score: mev_safety,
                composite_score,
                dominant_dex: dominant_dex.to_string(),
            }
        })
        .collect();

    info!(
        "🏆 Ranked {} candidate wallets — returning top {}",
        profiles.values().filter(|p| !p.disqualified).count(),
        top.len()
    );

    top
}

// ---------------------------------------------------------------------------
// Composite score
// ---------------------------------------------------------------------------

/// Composite = expectancy × momentum_bonus × mev_safety_factor
///
/// - `momentum_bonus`: 1.0 base, +0.2 if wallet is a momentum trader
/// - `mev_safety_factor`: 1.0 base, scaled down by herd ratio
///   (lower herd ratio = safer to copy)
fn compute_composite_score(profile: &WalletProfile) -> f64 {
    let base = profile.expectancy_score;

    // Momentum bonus: +20% if this wallet trades in the 5-15 min sweet spot
    let momentum_bonus: f64 = if profile.momentum_trader { 1.2 } else { 1.0 };

    // MEV safety: linearly interpolated from herd ratio
    // 0% herding → 1.0 (fully safe)
    // 100% herding → 0.0 (would be disqualified, but just in case)
    let mev_safety: f64 = 1.0 - profile.mev_herd_ratio;

    // Trade volume bonus: slight preference for more active wallets (log scale)
    let volume_factor = 1.0
        + (profile.cycles.len() as f64)
            .max(1.0)
            .ln()
            / 10.0;

    base * momentum_bonus * mev_safety * volume_factor
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the DEX used most often by this wallet.
fn find_dominant_dex(profile: &WalletProfile) -> &str {
    let mut counts: HashMap<&DexType, u32> = HashMap::new();
    for trade in &profile.trades {
        *counts.entry(&trade.dex_type).or_insert(0) += 1;
    }

    counts
        .into_iter()
        .max_by_key(|&(_, count)| count)
        .map(|(dex, _)| match dex {
            DexType::PumpFun => "PumpFun",
            DexType::PumpSwap => "PumpSwap",
            DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            DexType::Unknown => "Unknown",
        })
        .unwrap_or("Unknown")
}
