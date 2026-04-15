// shadow-radar/src/main.rs
//
// CLI entry point for the Shadow-Radar wallet profiler.
// Orchestrates the full pipeline:
//   gRPC replay → parse → aggregate → filter → rank → output
//
// This tool is READ-ONLY analytics — no transactions, no keys, no signing.

mod aggregator;
mod cache;
mod config;
mod db;
mod filters;
mod grpc_streamer;
mod heuristics;
mod models;
mod output;
mod ranker;
mod token_registry;
mod transaction_parser;
mod wallet_state;

use crate::config::{CliArgs, ShadowConfig};

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

// Channel buffer size — large enough to absorb gRPC burst without backpressure stalls
const TRADE_CHANNEL_SIZE: usize = 100_000;

#[tokio::main]
async fn main() -> Result<()> {
    // --- Parse CLI args ---
    let cli = CliArgs::parse();

    // --- Init tracing ---
    let filter = if cli.verbose {
        EnvFilter::new("shadow_radar=debug,info")
    } else {
        EnvFilter::new("shadow_radar=info,warn")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // --- Banner ---
    print_banner();

    // --- Load config ---
    let config = ShadowConfig::load(cli).context("Failed to load configuration")?;
    let config = Arc::new(config);

    // --- Mode dispatch ---
    match config.cli.mode.as_str() {
        "live" => run_live_mode(config).await,
        "replay" => run_replay_mode(config).await,
        other => {
            anyhow::bail!(
                "Unknown mode '{}'. Use --mode live or --mode replay",
                other
            );
        }
    }
}

// ===========================================================================
// REPLAY MODE — existing batch analysis pipeline (preserved as-is)
// ===========================================================================

async fn run_replay_mode(config: Arc<ShadowConfig>) -> Result<()> {
    tracing::info!(
        "⚙️  Config: {}h replay | min_trades={} | min_cycles={} | top={}",
        config.cli.hours,
        config.cli.min_trades,
        config.cli.min_cycles,
        config.cli.top
    );
    tracing::info!(
        "⚙️  Filter A: trade size [{:.1}, {:.1}] SOL",
        config.min_avg_trade_sol,
        config.max_avg_trade_sol
    );
    tracing::info!(
        "⚙️  Filter B: MEV herd threshold ≥{} copycats in ≥{:.0}% of buys",
        config.mev_follower_threshold,
        config.mev_herd_ratio_threshold * 100.0
    );
    tracing::info!(
        "⚙️  Filter C: momentum window [{}-{}]s | sniper ≤{} slots",
        config.momentum_entry_min_secs,
        config.momentum_entry_max_secs,
        config.sniper_max_slot_delta
    );
    tracing::info!(
        "⚙️  Filter D: win_rate ≥{:.0}% | avg_gain ≥{:.0}%",
        config.min_win_rate * 100.0,
        config.min_avg_gain_pct
    );

    // --- Phase 1: Stream historical trades ---
    println!(
        "\n{}",
        "═══ Phase 1: Streaming Historical DEX Data ═══"
            .cyan()
            .bold()
    );

    let (tx, rx) = mpsc::channel(TRADE_CHANNEL_SIZE);
    let stream_config = config.clone();

    let stream_handle = tokio::spawn(async move {
        grpc_streamer::stream_historical_trades(stream_config, tx).await
    });

    // --- Phase 2: Aggregate trades into wallet profiles ---
    println!(
        "\n{}",
        "═══ Phase 2: Aggregating Wallet Profiles ═══"
            .cyan()
            .bold()
    );

    let agg_result = aggregator::aggregate_trades(rx, &config).await;

    // Wait for streamer to finish (it may have already ended when channel closed)
    let _stream_stats = stream_handle
        .await
        .context("Stream task panicked")?
        .context("Stream failed")?;

    // --- Phase 3: Apply filter pipeline ---
    println!(
        "\n{}",
        "═══ Phase 3: Applying Filter Pipeline ═══"
            .cyan()
            .bold()
    );

    let mut profiles = agg_result.profiles;
    let profiles_built = profiles.len();

    filters::apply_all_filters(&mut profiles, &agg_result.token_trades, &config);

    let wallets_passing = profiles.values().filter(|p| !p.disqualified).count();

    // --- Phase 4: Rank and select top wallets ---
    println!(
        "\n{}",
        "═══ Phase 4: Ranking Alpha Wallets ═══"
            .cyan()
            .bold()
    );

    let top_wallets = ranker::rank_wallets(&profiles, &config);

    // --- Phase 5: Output ---
    println!(
        "\n{}",
        "═══ Phase 5: Generating Report ═══".cyan().bold()
    );

    output::print_summary(
        agg_result.total_trades,
        agg_result.total_wallets,
        profiles_built,
        wallets_passing,
        top_wallets.len(),
    );

    output::print_report(&top_wallets, config.cli.hours);

    output::write_report(
        &top_wallets,
        &config.cli.output,
        config.cli.hours,
        agg_result.total_trades,
        agg_result.total_wallets,
        wallets_passing as u64,
    )?;

    println!(
        "{}",
        "✅ Shadow-Radar scan complete. Stay surgical. 🎯"
            .green()
            .bold()
    );

    Ok(())
}

// ===========================================================================
// LIVE MODE — continuous daemon with heuristic filtering + SQLite output
// ===========================================================================

async fn run_live_mode(config: Arc<ShadowConfig>) -> Result<()> {
    tracing::info!(
        "{}",
        "🔴 LIVE MODE: Stealth Insider Discovery Daemon".red().bold()
    );
    tracing::info!(
        "⚙️  DB: {} | Anti-sniper window: [{}, {}] slots | Win rate ≥80%",
        config.cli.db,
        3, 15
    );

    // --- Initialize SQLite database (WAL mode) ---
    let target_db = db::TargetDb::open(&config.cli.db)
        .context("Failed to open target database")?;
    let target_db = Arc::new(tokio::sync::Mutex::new(target_db));

    // --- Initialize in-memory cache ---
    let insider_cache = Arc::new(cache::InsiderCache::with_defaults());
    let heuristic_config = Arc::new(heuristics::HeuristicConfig::default());

    // --- Shutdown signal ---
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // --- Trade channel ---
    let (trade_tx, mut trade_rx) = mpsc::channel::<models::ParsedTrade>(TRADE_CHANNEL_SIZE);

    // ======================================================================
    // Task 1: gRPC Streamer — streams live trades into the channel
    // ======================================================================
    let stream_config = config.clone();
    let stream_shutdown = shutdown_rx.clone();
    let streamer_handle = tokio::spawn(async move {
        grpc_streamer::stream_live_trades(stream_config, trade_tx, stream_shutdown).await
    });

    // ======================================================================
    // Task 2: Trade Processor — consumes trades, updates cache, evaluates
    // ======================================================================
    let proc_cache = insider_cache.clone();
    let proc_db = target_db.clone();
    let proc_heuristic_config = heuristic_config.clone();
    let proc_shutdown = shutdown_rx.clone();

    let processor_handle = tokio::spawn(async move {
        let mut qualified_count: u64 = 0;

        while let Some(trade) = trade_rx.recv().await {
            if *proc_shutdown.borrow() {
                break;
            }

            // Process through cache pipeline (T₀ tracking, annotation, state update)
            let result = proc_cache.process_trade(trade).await;

            // Evaluate heuristics for the updated wallet
            if let Some(state) = proc_cache.get_wallet_state(&result.wallet).await {
                // Only evaluate wallets with sufficient data
                if state.has_sufficient_data() {
                    let eval = heuristics::evaluate_wallet(
                        &state,
                        &proc_cache,
                        &proc_heuristic_config,
                    )
                    .await;

                    if eval.qualified {
                        qualified_count += 1;

                        // Compute composite score
                        let score = state.win_rate_last_20 * 4.0
                            + state.insider_window_ratio * 3.0
                            + (1.0 - state.avg_max_single_sell_pct) * 2.0
                            + (1.0 - (state.avg_entry_size_sol / 5.0).min(1.0));

                        let row = db::QualifiedWalletRow {
                            address: state.address.clone(),
                            win_rate: state.win_rate_last_20,
                            avg_entry_sol: state.avg_entry_size_sol,
                            avg_sell_tranches: state.avg_sell_tranches,
                            insider_window_ratio: state.insider_window_ratio,
                            composite_score: score,
                            total_cycles: state.completed_cycles.len() as u32,
                            total_trades: state.lifetime_trade_count,
                            last_updated: state.last_updated,
                            qualified_at: state.last_updated,
                        };

                        // Write to SQLite (blocking but fast with WAL mode)
                        if let Ok(db_lock) = proc_db.try_lock() {
                            if let Err(e) = db_lock.upsert_qualified_wallet(&row) {
                                tracing::error!("Failed to upsert wallet: {}", e);
                            }
                            if let Err(e) = db_lock.log_qualification(&state.address, score) {
                                tracing::error!("Failed to log qualification: {}", e);
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            "🏁 Trade processor stopped. {} wallets qualified total.",
            qualified_count
        );
    });

    // ======================================================================
    // Task 3: Maintenance — periodic cache eviction, stats, trade pruning
    // ======================================================================
    let maint_cache = insider_cache.clone();
    let maint_db = target_db.clone();
    let maint_shutdown = shutdown_rx.clone();

    let maintenance_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300)); // 5 min

        loop {
            interval.tick().await;

            if *maint_shutdown.borrow() {
                break;
            }

            // Run cache maintenance (evict expired entries, log stats)
            maint_cache.run_maintenance().await;

            // Prune old trades from SQLite (keep last 48h)
            if let Ok(db_lock) = maint_db.try_lock() {
                if let Err(e) = db_lock.prune_old_trades(48) {
                    tracing::error!("Failed to prune old trades: {}", e);
                }
                // Demote wallets not updated in 6 hours
                if let Err(e) = db_lock.demote_stale_wallets(6 * 3600) {
                    tracing::error!("Failed to demote stale wallets: {}", e);
                }
                if let Ok(count) = db_lock.active_wallet_count() {
                    tracing::info!("🎯 Active qualified wallets in DB: {}", count);
                }
            }
        }
    });

    // ======================================================================
    // Ctrl+C handler
    // ======================================================================
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        tracing::info!("🛑 Ctrl+C received — initiating graceful shutdown...");
        let _ = shutdown_tx.send(true);
    });

    // --- Wait for all tasks ---
    tokio::select! {
        res = streamer_handle => {
            if let Err(e) = res {
                tracing::error!("Streamer task failed: {}", e);
            }
        }
        res = processor_handle => {
            if let Err(e) = res {
                tracing::error!("Processor task failed: {}", e);
            }
        }
        res = maintenance_handle => {
            if let Err(e) = res {
                tracing::error!("Maintenance task failed: {}", e);
            }
        }
    }

    println!(
        "{}",
        "✅ Shadow-Radar daemon stopped. Stay surgical. 🎯"
            .green()
            .bold()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Banner
// ---------------------------------------------------------------------------

fn print_banner() {
    let banner = r#"
    ███████╗██╗  ██╗ █████╗ ██████╗  ██████╗ ██╗    ██╗
    ██╔════╝██║  ██║██╔══██╗██╔══██╗██╔═══██╗██║    ██║
    ███████╗███████║███████║██║  ██║██║   ██║██║ █╗ ██║
    ╚════██║██╔══██║██╔══██║██║  ██║██║   ██║██║███╗██║
    ███████║██║  ██║██║  ██║██████╔╝╚██████╔╝╚███╔███╔╝
    ╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝  ╚═════╝  ╚══╝╚══╝ 
              ██████╗  █████╗ ██████╗  █████╗ ██████╗
              ██╔══██╗██╔══██╗██╔══██╗██╔══██╗██╔══██╗
              ██████╔╝███████║██║  ██║███████║██████╔╝
              ██╔══██╗██╔══██║██║  ██║██╔══██║██╔══██╗
              ██║  ██║██║  ██║██████╔╝██║  ██║██║  ██║
              ╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝
    "#;
    println!("{}", banner.cyan().bold());
    println!(
        "{}",
        "    Advanced Solana Wallet Profiler — Smart Money Radar"
            .white()
            .bold()
    );
    println!(
        "{}",
        "    Read-only analytics • No keys • No transactions"
            .white()
            .dimmed()
    );
    println!();
}
