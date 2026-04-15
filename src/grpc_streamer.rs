// shadow-radar/src/grpc_streamer.rs
//
// Yellowstone gRPC historical-replay streamer.
// Connects to the gRPC endpoint, subscribes to DEX transactions over a
// configurable time window (12-24h), and pushes parsed trades through an
// mpsc channel for downstream aggregation.
//
// Implements heartbeat pings and reconnection following sniper-bot patterns.

use crate::config::ShadowConfig;
use crate::models::ParsedTrade;
use crate::transaction_parser;

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeRequestPing,
};

/// Maximum gRPC connection retries before giving up.
const MAX_CONNECT_RETRIES: u32 = 5;
/// Delay between connection retries.
const RETRY_DELAY_SECS: u64 = 5;
/// Heartbeat ping interval.
const HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Stream historical DEX transactions and push parsed trades into `tx`.
///
/// This function blocks until the full replay window has been consumed or
/// a fatal error occurs.
pub async fn stream_historical_trades(
    config: Arc<ShadowConfig>,
    tx: mpsc::Sender<ParsedTrade>,
) -> Result<StreamStats> {
    let mut stats = StreamStats::default();

    info!(
        "{}",
        format!(
            "🛰️  Connecting to Yellowstone gRPC: {}",
            &config.yellowstone_grpc_http
        )
        .cyan()
    );

    // --- Connect with retries ---
    let mut client = {
        let mut retries = 0;
        loop {
            match GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
                .context("Failed to build gRPC client")?
                .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
                .context("Failed to set x_token")?
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .context("Failed to set TLS config")?
                .connect()
                .await
            {
                Ok(c) => break c,
                Err(e) => {
                    retries += 1;
                    if retries >= MAX_CONNECT_RETRIES {
                        return Err(e).context(format!(
                            "Failed to connect after {} attempts",
                            MAX_CONNECT_RETRIES
                        ));
                    }
                    warn!(
                        "Connection attempt {}/{} failed: {}. Retrying in {}s...",
                        retries, MAX_CONNECT_RETRIES, e, RETRY_DELAY_SECS
                    );
                    time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                }
            }
        }
    };

    // --- Subscribe ---
    let (subscribe_tx, mut stream) = {
        let mut retries = 0;
        loop {
            match client.subscribe().await {
                Ok(pair) => break pair,
                Err(e) => {
                    retries += 1;
                    if retries >= MAX_CONNECT_RETRIES {
                        return Err(e).context("Failed to subscribe after retries");
                    }
                    warn!("Subscribe attempt {}/{} failed. Retrying...", retries, MAX_CONNECT_RETRIES);
                    time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                }
            }
        }
    };

    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

    // --- Build subscription filter for DEX programs ---
    let dex_programs = vec![
        transaction_parser::PUMP_FUN_PROGRAM.to_string(),
        transaction_parser::PUMP_SWAP_PROGRAM.to_string(),
        transaction_parser::RAYDIUM_LAUNCHPAD_PROGRAM.to_string(),
    ];

    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "dex_swaps".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: dex_programs,
                account_exclude: vec![],
                account_required: vec![],
            }
        },
        commitment: Some(CommitmentLevel::Confirmed as i32),
        ..Default::default()
    };

    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .context("Failed to send subscribe request")?;

    info!("{}", "✅ gRPC subscription active — streaming DEX transactions".green());

    // --- Heartbeat task ---
    let hb_tx = subscribe_tx.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            interval.tick().await;
            let ping = SubscribeRequest {
                ping: Some(SubscribeRequestPing { id: 0 }),
                ..Default::default()
            };
            let mut guard = hb_tx.lock().await;
            if guard.send(ping).await.is_err() {
                debug!("Heartbeat channel closed — stopping pings");
                break;
            }
        }
    });

    // --- Compute time boundary ---
    let now_ts = Utc::now().timestamp() as u64;
    let window_secs = config.cli.hours.saturating_mul(3600);
    let cutoff_ts = now_ts.saturating_sub(window_secs);

    info!(
        "📡 Replay window: last {} hours (cutoff timestamp: {})",
        config.cli.hours, cutoff_ts
    );

    // --- Progress bar ---
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] {msg}  |  {prefix}"
        )
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
    );
    pb.set_prefix("trades: 0  |  slots: 0");

    let mut last_slot: u64 = 0;

    // --- Main stream processing ---
    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(update)) => {
                        match update.update_oneof {
                            Some(UpdateOneof::Transaction(txn_update)) => {
                                let slot = txn_update.slot;

                                // Track progress
                                if slot > last_slot {
                                    last_slot = slot;
                                    stats.slots_processed = stats.slots_processed.saturating_add(1);
                                }
                                stats.transactions_seen = stats.transactions_seen.saturating_add(1);

                                // Parse all DEX events from this transaction
                                let trades = transaction_parser::parse_all_events(&txn_update);

                                for trade in trades {
                                    // Apply time-window filter: skip trades older than cutoff
                                    if trade.timestamp > 0 && trade.timestamp < cutoff_ts {
                                        continue;
                                    }

                                    stats.trades_parsed = stats.trades_parsed.saturating_add(1);

                                    if tx.send(trade).await.is_err() {
                                        warn!("Trade channel closed — consumer disconnected");
                                        break;
                                    }
                                }

                                // Update progress bar
                                if stats.transactions_seen % 500 == 0 {
                                    pb.set_prefix(format!(
                                        "trades: {}  |  slots: {}",
                                        stats.trades_parsed, stats.slots_processed
                                    ));
                                    pb.set_message(format!(
                                        "txns scanned: {}  |  last slot: {}",
                                        stats.transactions_seen, last_slot
                                    ));
                                    pb.tick();
                                }
                            }
                            Some(UpdateOneof::Ping(_)) => {
                                debug!("Received pong from server");
                            }
                            _ => {
                                // Ignore other update types
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!("gRPC stream error: {:?}", e);
                        break;
                    }
                    None => {
                        info!("gRPC stream ended (replay complete)");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C — shutting down stream gracefully");
                break;
            }
        }
    }

    // --- Cleanup ---
    heartbeat_handle.abort();
    pb.finish_with_message(format!(
        "Stream complete: {} trades from {} transactions across {} slots",
        stats.trades_parsed, stats.transactions_seen, stats.slots_processed
    ));

    info!(
        "{}",
        format!(
            "📊 Stream stats: {} DEX trades parsed from {} transactions across {} slots",
            stats.trades_parsed, stats.transactions_seen, stats.slots_processed
        )
        .green()
    );

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub transactions_seen: u64,
    pub trades_parsed: u64,
    pub slots_processed: u64,
}
