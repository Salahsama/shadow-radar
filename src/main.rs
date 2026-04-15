// shadow-radar/src/main.rs
//
// CLI entry point for the Shadow-Radar wallet profiler.
// Orchestrates the full pipeline:
//   gRPC replay → parse → aggregate → filter → rank → output
//
// This tool is READ-ONLY analytics — no transactions, no keys, no signing.

mod aggregator;
mod config;
mod filters;
mod grpc_streamer;
mod models;
mod output;
mod ranker;
mod transaction_parser;

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
