// shadow-radar/src/output.rs
//
// JSON file output and CLI table display for the alpha wallet report.

use crate::models::{AlphaWallet, AlphaWalletReport};

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use std::path::Path;

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

/// Build the final report and write it to disk as JSON.
pub fn write_report(
    wallets: &[AlphaWallet],
    output_path: &str,
    replay_hours: u64,
    total_trades: u64,
    total_wallets: u64,
    wallets_after_filters: u64,
) -> Result<()> {
    let report = AlphaWalletReport {
        generated_at: Utc::now().to_rfc3339(),
        replay_window_hours: replay_hours,
        total_trades_scanned: total_trades,
        total_wallets_seen: total_wallets,
        wallets_after_filters,
        top_wallets: wallets.to_vec(),
    };

    let json = serde_json::to_string_pretty(&report)
        .context("Failed to serialize report to JSON")?;

    std::fs::write(Path::new(output_path), &json)
        .context(format!("Failed to write report to {}", output_path))?;

    println!(
        "\n{}",
        format!("📁 Report saved to: {}", output_path)
            .green()
            .bold()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI table display
// ---------------------------------------------------------------------------

/// Print a beautifully formatted CLI table of the alpha wallets.
pub fn print_report(wallets: &[AlphaWallet], replay_hours: u64) {
    let bar = "═".repeat(66);
    let thin = "─".repeat(66);

    println!();
    println!("{}", format!("╔{}╗", bar).cyan().bold());
    println!(
        "{}",
        format!(
            "║  {} :: Alpha Wallet Report  ({}h window){} ║",
            "SHADOW-RADAR".bold(),
            replay_hours,
            " ".repeat(66 - 50 - replay_hours.to_string().len())
        )
        .cyan()
        .bold()
    );
    println!("{}", format!("╠{}╣", bar).cyan().bold());

    if wallets.is_empty() {
        println!(
            "{}",
            format!(
                "║  {} ║",
                "No wallets passed all filters. Try relaxing thresholds.                "
                    .yellow()
            )
            .cyan()
        );
        println!("{}", format!("╚{}╝", bar).cyan().bold());
        return;
    }

    for wallet in wallets {
        let _addr_short = if wallet.address.len() > 8 {
            format!(
                "{}...{}",
                &wallet.address[..4],
                &wallet.address[wallet.address.len() - 4..]
            )
        } else {
            wallet.address.clone()
        };

        // Rank line
        let rank_str = format!("#{}", wallet.rank);
        let rank_line = format!(
            "║  {}  {}",
            rank_str.green().bold(),
            wallet.address.white().bold()
        );
        println!("{}", rank_line);

        // Metrics line 1
        println!(
            "{}",
            format!(
                "║     Win Rate: {}  │  Avg Size: {} SOL  │  Cycles: {}",
                format!("{:.1}%", wallet.win_rate * 100.0).green(),
                format!("{:.3}", wallet.avg_trade_size_sol).yellow(),
                format!("{}", wallet.completed_cycles).white(),
            )
            .cyan()
        );

        // Metrics line 2
        println!(
            "{}",
            format!(
                "║     Expectancy: {}  │  Momentum: {}  │  MEV Safety: {}",
                format!("{:.2}", wallet.expectancy_score).green().bold(),
                format!("{:.2}", wallet.momentum_score).yellow(),
                format!("{:.2}", wallet.mev_safety_score).green(),
            )
            .cyan()
        );

        // Metrics line 3
        println!(
            "{}",
            format!(
                "║     Avg Gain: {}  │  Avg Loss: {}  │  DEX: {}",
                format!("+{:.1}%", wallet.avg_gain_pct).green(),
                format!("-{:.1}%", wallet.avg_loss_pct).red(),
                wallet.dominant_dex.white(),
            )
            .cyan()
        );

        // Composite score
        println!(
            "{}",
            format!(
                "║     ▸ Composite Score: {}",
                format!("{:.4}", wallet.composite_score).green().bold(),
            )
            .cyan()
        );

        println!("{}", format!("║  {}", thin).cyan());
    }

    println!("{}", format!("╚{}╝", bar).cyan().bold());
    println!();
}

/// Print a summary of the pipeline run.
pub fn print_summary(
    total_trades: u64,
    total_wallets: u64,
    profiles_built: usize,
    wallets_passing: usize,
    top_count: usize,
) {
    println!();
    println!("{}", "─── Pipeline Summary ───".cyan().bold());
    println!(
        "  {} DEX trades scanned",
        format!("{}", total_trades).white().bold()
    );
    println!(
        "  {} unique wallets observed",
        format!("{}", total_wallets).white().bold()
    );
    println!(
        "  {} wallet profiles built (passed min-trades)",
        format!("{}", profiles_built).white().bold()
    );
    println!(
        "  {} wallets survived all 4 filters",
        format!("{}", wallets_passing).green().bold()
    );
    println!(
        "  {} alpha wallets selected",
        format!("{}", top_count).green().bold()
    );
    println!();
}
