// shadow-radar/src/transaction_parser.rs
//
// DEX event buffer parser — adapted from solana-sniper-bot/src/processor/transaction_parser.rs
// Supports PumpFun/PumpSwap events via discriminator-based parsing (handles variable-length
// buffers), and Raydium Launchpad events.
//
// Security: Uses checked arithmetic throughout per solana-dev-skill security checklist.

use crate::models::{DexType, ParsedTrade};
use tracing::{debug, trace};
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;

// ---------------------------------------------------------------------------
// Known program IDs
// ---------------------------------------------------------------------------

/// PumpFun bonding-curve program
pub const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// PumpSwap AMM program
pub const PUMP_SWAP_PROGRAM: &str = "PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP";
/// Raydium Launchpad program
pub const RAYDIUM_LAUNCHPAD_PROGRAM: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";
/// Raydium AMM V4 program
pub const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Wrapped SOL mint (used to identify SOL legs in token balance parsing)
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// ---------------------------------------------------------------------------
// Sub-discriminators (bytes 8..16 of the event buffer)
// ---------------------------------------------------------------------------

/// PumpSwap swap event sub-discriminator
/// All swap events (buy, sell, buy_exact_sol_in, etc.) share this 8-byte tag.
const PUMPSWAP_SWAP_DISCRIMINATOR: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];

/// PumpSwap trade/pool log event sub-discriminator (384-byte events with pool reserves)
const PUMPSWAP_TRADE_LOG_DISCRIMINATOR: [u8; 8] = [0x3e, 0x2f, 0x37, 0x0a, 0xa5, 0x03, 0xdc, 0x2a];

/// PumpSwap fee distribution event sub-discriminator (215-byte events — skip these)
const PUMPSWAP_FEE_DISCRIMINATOR: [u8; 8] = [0x31, 0x48, 0x7b, 0x2d, 0x6e, 0x40, 0xb0, 0x85];

/// Main event discriminator shared by all PumpSwap/PumpFun events
const MAIN_DISCRIMINATOR: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

// ---------------------------------------------------------------------------
// Helpers — safe buffer parsing
// ---------------------------------------------------------------------------

#[inline]
fn parse_pubkey(buffer: &[u8], offset: usize) -> Option<String> {
    if offset.checked_add(32)? > buffer.len() {
        return None;
    }
    Some(bs58::encode(&buffer[offset..offset + 32]).into_string())
}

#[inline]
fn parse_u64(buffer: &[u8], offset: usize) -> Option<u64> {
    if offset.checked_add(8)? > buffer.len() {
        return None;
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buffer[offset..offset + 8]);
    Some(u64::from_le_bytes(bytes))
}

#[inline]
fn parse_u8(buffer: &[u8], offset: usize) -> Option<u8> {
    buffer.get(offset).copied()
}

// ---------------------------------------------------------------------------
// Log-message sniffing for buy/sell direction
// ---------------------------------------------------------------------------

fn has_buy_instruction(txn: &SubscribeUpdateTransaction) -> bool {
    if let Some(tx_inner) = &txn.transaction {
        if let Some(meta) = &tx_inner.meta {
            return meta
                .log_messages
                .iter()
                .any(|log| log.contains("Program log: Instruction: Buy"));
        }
    }
    false
}

fn has_sell_instruction(txn: &SubscribeUpdateTransaction) -> bool {
    if let Some(tx_inner) = &txn.transaction {
        if let Some(meta) = &tx_inner.meta {
            return meta
                .log_messages
                .iter()
                .any(|log| log.contains("Program log: Instruction: Sell"));
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Token mint extraction from post-token-balances
// ---------------------------------------------------------------------------

fn extract_token_mint(txn: &SubscribeUpdateTransaction) -> String {
    if let Some(tx_inner) = &txn.transaction {
        if let Some(meta) = &tx_inner.meta {
            if !meta.post_token_balances.is_empty() {
                let mut mint = meta.post_token_balances[0].mint.clone();
                // Skip WSOL entries
                if mint == "So11111111111111111111111111111111111111112"
                    && meta.post_token_balances.len() > 1
                {
                    mint = meta.post_token_balances[1].mint.clone();
                    if mint == "So11111111111111111111111111111111111111112"
                        && meta.post_token_balances.len() > 2
                    {
                        mint = meta.post_token_balances[2].mint.clone();
                    }
                }
                if !mint.is_empty() {
                    return mint;
                }
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Signer extraction (fee payer = wallet address)
// ---------------------------------------------------------------------------

/// Extract the transaction signer (fee payer / wallet) from account_keys[0].
pub fn extract_signer(txn: &SubscribeUpdateTransaction) -> Option<String> {
    let transaction_info = txn.transaction.as_ref()?;
    let transaction = transaction_info.transaction.as_ref()?;
    let message = transaction.message.as_ref()?;
    if message.account_keys.is_empty() {
        return None;
    }
    Some(bs58::encode(&message.account_keys[0]).into_string())
}

// ---------------------------------------------------------------------------
// Direction extraction from the tail ASCII string
// ---------------------------------------------------------------------------

/// Parse the direction string from the end of the PumpSwap event buffer.
/// The buffer ends with a length-prefixed UTF-8 string like:
///   u32_le(3) + "buy"
///   u32_le(4) + "sell"
///   u32_le(16) + "buy_exact_sol_in"
///   etc.
/// Returns Some(true) for buy, Some(false) for sell, None if unparseable.
fn parse_direction_from_tail(buffer: &[u8]) -> Option<bool> {
    // The direction string is at the very end.
    // Try to find common patterns by scanning backwards for known strings.
    let buf_str = String::from_utf8_lossy(buffer);

    if buf_str.contains("buy") {
        return Some(true);
    }
    if buf_str.contains("sell") {
        return Some(false);
    }

    None
}

// ---------------------------------------------------------------------------
// Main event-buffer parser
// ---------------------------------------------------------------------------

/// Attempt to parse a DEX swap event from an inner-instruction data buffer.
///
/// Returns `Some(ParsedTrade)` on success. The caller must fill in `slot`,
/// `signature`, and `tx_index` from the outer transaction envelope.
pub fn parse_event_buffer(
    txn: &SubscribeUpdateTransaction,
    buffer: &[u8],
) -> Option<ParsedTrade> {
    let wallet = extract_signer(txn)?;

    // Need at least 16 bytes for the main discriminator + sub-discriminator
    if buffer.len() < 16 {
        return None;
    }

    // Check for the main PumpSwap/PumpFun event discriminator
    if buffer[0..8] == MAIN_DISCRIMINATOR {
        let sub_disc: [u8; 8] = buffer[8..16].try_into().ok()?;

        match sub_disc {
            // ============================================================
            // PumpSwap Swap Events (variable size: 155..304+ bytes)
            // ============================================================
            d if d == PUMPSWAP_SWAP_DISCRIMINATOR => {
                return parse_pumpswap_swap(buffer, &wallet, txn);
            }

            // ============================================================
            // PumpSwap Trade Log (384 bytes — pool reserve snapshot)
            // ============================================================
            d if d == PUMPSWAP_TRADE_LOG_DISCRIMINATOR => {
                return parse_pumpswap_trade_log(buffer, &wallet, txn);
            }

            // ============================================================
            // Fee distribution events (215 bytes) — skip
            // ============================================================
            d if d == PUMPSWAP_FEE_DISCRIMINATOR => {
                trace!("Skipping PumpSwap fee event ({}B)", buffer.len());
                return None;
            }

            // Unknown sub-discriminator — might be PumpFun CreateV2 or other events
            _ => {
                trace!(
                    "Unknown sub-discriminator {:02x?} ({}B)",
                    &sub_disc,
                    buffer.len()
                );
                return None;
            }
        }
    }

    // ====================================================================
    // Legacy PumpFun events (matched by buffer size)
    // These don't use the main discriminator prefix.
    // ====================================================================
    match buffer.len() {
        // PumpFun — 266 bytes (standard bonding curve event)
        266 => parse_pumpfun_266(buffer, &wallet, txn),

        // PumpFun — 170 bytes (alternate variant)
        170 => parse_pumpfun_short(buffer, &wallet, txn),

        // PumpFun — 138 bytes (alternate variant)
        138 => parse_pumpfun_short(buffer, &wallet, txn),

        // Raydium Launchpad — 146 bytes
        146 => parse_raydium_146(buffer, &wallet, txn),

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PumpSwap Swap Event Parser
// ---------------------------------------------------------------------------
// Buffer layout (confirmed from live diagnostic data, April 2026):
//
//   [0..8]   Main discriminator: e4 45 a5 2e 51 cb 9a 1d
//   [8..16]  Sub-discriminator:  bd db 7f d3 4e e6 61 ee
//   [16..48] Token mint (32-byte pubkey)
//   [48..56] SOL amount (lamports, u64 LE)
//   [56..64] Token amount (raw units, u64 LE)
//   [64..96] Pool/user pubkey (32 bytes) — pool account or user ATA
//   [96..104] Timestamp (u64 LE, unix seconds)
//   [104..112] Additional amount field (fees, etc.)
//   [112..144] Another pubkey (32 bytes)
//   [144..176] Another pubkey (32 bytes)
//   [176..Npad] Padding/additional data
//   [N-varies..end] Length-prefixed direction string: "buy", "sell", "buy_exact_sol_in", etc.
//
// Size varies:
//   - 290 bytes → "buy" (3 chars)
//   - 291 bytes → "sell" (4 chars)
//   - 303 bytes → "buy_exact_sol_in" (16 chars)
//   - 304 bytes → other variant
//   - 155 bytes → compact swap event (shorter layout)

fn parse_pumpswap_swap(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    // Minimum: need 16 (discriminators) + 32 (mint) + 8 (sol_amount) + 8 (token_amount) = 64
    if buffer.len() < 64 {
        return None;
    }

    // For compact events (155 bytes), the layout is different.
    // For standard events (>= 260 bytes), we use the standard layout.
    if buffer.len() >= 260 {
        return parse_pumpswap_swap_standard(buffer, wallet, txn);
    }

    // Compact swap event (155 bytes etc.)
    parse_pumpswap_swap_compact(buffer, wallet, txn)
}

/// Parse standard PumpSwap swap events (260+ bytes)
fn parse_pumpswap_swap_standard(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    // Extract token mint from the embedded pubkey at offset 16
    let mint = parse_pubkey(buffer, 16)?;

    // Skip if mint looks like WSOL
    let actual_mint = if mint == "So11111111111111111111111111111111111111112" {
        // Fall back to post_token_balances
        let fallback = extract_token_mint(txn);
        if fallback.is_empty() {
            return None;
        }
        fallback
    } else {
        mint
    };

    // SOL amount in lamports
    let sol_amount_lamports = parse_u64(buffer, 48)?;
    let sol_amount = sol_amount_lamports as f64 / 1_000_000_000.0;

    // Token amount
    let token_amount_raw = parse_u64(buffer, 56)?;
    let token_amount = token_amount_raw as f64 / 1_000_000.0; // SPL tokens use 6 decimal places

    // Determine buy/sell by parsing the direction string from the buffer tail
    let is_buy = match parse_direction_from_tail(buffer) {
        Some(dir) => dir,
        None => {
            // Fallback to log message sniffing
            if has_buy_instruction(txn) {
                true
            } else if has_sell_instruction(txn) {
                false
            } else {
                // Default to buy if we can't determine direction
                debug!(
                    "Could not determine direction for swap ({}B), defaulting to buy",
                    buffer.len()
                );
                true
            }
        }
    };

    // Try to get timestamp from the buffer
    // Check multiple possible offsets for a valid unix timestamp
    let timestamp = find_timestamp_in_buffer(buffer)
        .unwrap_or_else(|| current_unix_timestamp());

    // Compute a rough price from sol/token amounts
    let price = if token_amount > 0.0 {
        ((sol_amount / token_amount) * 1_000_000_000.0) as u64
    } else {
        0
    };

    trace!(
        "PumpSwap-{} {} {} — {:.6} SOL | {:.2} tokens",
        buffer.len(),
        if is_buy { "BUY" } else { "SELL" },
        &actual_mint[..8.min(actual_mint.len())],
        sol_amount,
        token_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: actual_mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpSwap,
        signature: String::new(),
        tx_index: 0,
    })
}

/// Parse compact PumpSwap swap events (< 260 bytes, e.g. 155 bytes)
fn parse_pumpswap_swap_compact(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    // For compact events, the mint is embedded at offset 16..48
    let mint = parse_pubkey(buffer, 16)?;

    let actual_mint = if mint == "So11111111111111111111111111111111111111112" {
        let fallback = extract_token_mint(txn);
        if fallback.is_empty() {
            return None;
        }
        fallback
    } else {
        mint
    };

    // In compact events, SOL amount is at offset 48
    let sol_amount_lamports = parse_u64(buffer, 48)?;
    // Validate: skip if the SOL amount doesn't look like a realistic trade
    // (should be < 1000 SOL = 1_000_000_000_000 lamports)
    if sol_amount_lamports > 1_000_000_000_000 {
        // This offset doesn't contain a real SOL amount, try different layout
        return parse_pumpswap_swap_compact_alt(buffer, wallet, txn, &actual_mint);
    }

    let sol_amount = sol_amount_lamports as f64 / 1_000_000_000.0;

    // Token amount at offset 56
    let token_amount_raw = parse_u64(buffer, 56)?;
    let token_amount = token_amount_raw as f64 / 1_000_000.0;

    // Determine direction
    let is_buy = match parse_direction_from_tail(buffer) {
        Some(dir) => dir,
        None => has_buy_instruction(txn),
    };

    let timestamp = find_timestamp_in_buffer(buffer)
        .unwrap_or_else(|| current_unix_timestamp());

    let price = if token_amount > 0.0 {
        ((sol_amount / token_amount) * 1_000_000_000.0) as u64
    } else {
        0
    };

    trace!(
        "PumpSwap-compact-{} {} {} — {:.6} SOL",
        buffer.len(),
        if is_buy { "BUY" } else { "SELL" },
        &actual_mint[..8.min(actual_mint.len())],
        sol_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: actual_mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpSwap,
        signature: String::new(),
        tx_index: 0,
    })
}

/// Alternative compact layout parser — tries different field offsets
fn parse_pumpswap_swap_compact_alt(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
    mint: &str,
) -> Option<ParsedTrade> {
    // For the compact 155-byte layout, the structure might be:
    // [16..48] mint, [48..56] fee/flags, [56..64] sol, [64..72] token, etc.
    // Or the amounts might be at different offsets.
    // Try to find two u64 fields that look like realistic trade amounts.

    let mut sol_amount = 0.0;
    let mut token_amount = 0.0;
    let mut found = false;

    // Scan for realistic SOL amounts (0.001 SOL to 1000 SOL)
    for off in (48..buffer.len().saturating_sub(8)).step_by(8) {
        if let Some(val) = parse_u64(buffer, off) {
            let as_sol = val as f64 / 1_000_000_000.0;
            if as_sol >= 0.001 && as_sol <= 1000.0 {
                sol_amount = as_sol;
                // Next field is likely token amount
                if let Some(tok_val) = parse_u64(buffer, off + 8) {
                    token_amount = tok_val as f64 / 1_000_000.0;
                }
                found = true;
                break;
            }
        }
    }

    if !found || sol_amount <= 0.0 {
        return None;
    }

    let is_buy = match parse_direction_from_tail(buffer) {
        Some(dir) => dir,
        None => has_buy_instruction(txn),
    };

    let timestamp = find_timestamp_in_buffer(buffer)
        .unwrap_or_else(|| current_unix_timestamp());

    let price = if token_amount > 0.0 {
        ((sol_amount / token_amount) * 1_000_000_000.0) as u64
    } else {
        0
    };

    trace!(
        "PumpSwap-compact-alt-{} {} {} — {:.6} SOL",
        buffer.len(),
        if is_buy { "BUY" } else { "SELL" },
        &mint[..8.min(mint.len())],
        sol_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: mint.to_string(),
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpSwap,
        signature: String::new(),
        tx_index: 0,
    })
}

// ---------------------------------------------------------------------------
// PumpSwap Trade Log Parser (384 bytes)
// ---------------------------------------------------------------------------
// This is a pool-level trade log event with full reserves.
//
// Layout (from diagnostic):
//   [0..8]   Main discriminator
//   [8..16]  Sub-discriminator: 3e 2f 37 0a a5 03 dc 2a
//   [16..24] Timestamp (u64 LE, unix seconds — confirmed!)
//   [24..32] base_amount (u64 LE, lamports)
//   [32..40] (padding/zero)
//   [40..48] pool_base_reserves (u64 LE)
//   [48..56] (padding/zero)
//   [56..64] pool_quote_reserves or similar
//   [64..72] quote_amount
//   [72..80] trade count or similar small number
//   ...remaining is pool state and pubkeys

fn parse_pumpswap_trade_log(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    if buffer.len() < 80 {
        return None;
    }

    let mint = extract_token_mint(txn);
    if mint.is_empty() {
        return None;
    }

    let timestamp = parse_u64(buffer, 16)?;
    // Validate timestamp
    if timestamp < 1_600_000_000 || timestamp > 2_000_000_000 {
        return None;
    }

    let base_amount = parse_u64(buffer, 24)?;

    // Find SOL amount from base_amount or quote_amount
    let sol_amount = base_amount as f64 / 1_000_000_000.0;

    // Sanity check: trade should be within reasonable range
    if sol_amount <= 0.0 || sol_amount > 10_000.0 {
        // base_amount is not the SOL amount, skip this
        return None;
    }

    let pool_base_reserves = parse_u64(buffer, 40).unwrap_or(0);
    let pool_quote_reserves = parse_u64(buffer, 56).unwrap_or(0);

    let is_buy = has_buy_instruction(txn) || !has_sell_instruction(txn);

    let price = if pool_base_reserves > 0 {
        pool_quote_reserves
            .checked_mul(1_000_000_000)?
            .checked_div(pool_base_reserves.max(1))?
    } else {
        0
    };

    let token_amount = 0.0; // Not directly available in this event format

    trace!(
        "PumpSwap-trade-log-{} {} {} — {:.6} SOL",
        buffer.len(),
        if is_buy { "BUY" } else { "SELL" },
        &mint[..8.min(mint.len())],
        sol_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpSwap,
        signature: String::new(),
        tx_index: 0,
    })
}

// ---------------------------------------------------------------------------
// Legacy PumpFun Parsers
// ---------------------------------------------------------------------------

/// PumpFun — 266 bytes (standard bonding curve event)
fn parse_pumpfun_266(
    buffer: &[u8],
    wallet: &str,
    _txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    let mint = parse_pubkey(buffer, 16)?;
    let sol_amount_raw = parse_u64(buffer, 48)?;
    let token_amount_raw = parse_u64(buffer, 56)?;
    let is_buy = buffer.get(64)? == &1;
    let timestamp = parse_u64(buffer, 97)?;
    let virtual_sol = parse_u64(buffer, 105)?;
    let virtual_token = parse_u64(buffer, 113)?;

    let price = virtual_sol
        .checked_mul(1_000_000_000)?
        .checked_div(virtual_token.max(1))?;

    let sol_amount = sol_amount_raw as f64 / 1_000_000_000.0;
    let token_amount = token_amount_raw as f64 / 1_000_000_000.0;

    trace!(
        "PumpFun-266 {} {} — {:.4} SOL",
        if is_buy { "BUY" } else { "SELL" },
        &mint[..8.min(mint.len())],
        sol_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpFun,
        signature: String::new(),
        tx_index: 0,
    })
}

/// Parse a "short" PumpFun event (170 or 138 bytes — same layout, just truncated).
fn parse_pumpfun_short(
    buffer: &[u8],
    wallet: &str,
    _txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    let mint = parse_pubkey(buffer, 16)?;
    let sol_amount_raw = parse_u64(buffer, 48)?;
    let token_amount_raw = parse_u64(buffer, 56)?;
    let is_buy = buffer.get(64)? == &1;
    let timestamp = parse_u64(buffer, 97)?;
    let virtual_sol = parse_u64(buffer, 105)?;
    let virtual_token = parse_u64(buffer, 113)?;

    let price = virtual_sol
        .checked_mul(1_000_000_000)?
        .checked_div(virtual_token.max(1))?;

    let sol_amount = sol_amount_raw as f64 / 1_000_000_000.0;
    let token_amount = token_amount_raw as f64 / 1_000_000_000.0;

    debug!(
        "PumpFun-{} {} {} — {:.4} SOL",
        buffer.len(),
        if is_buy { "BUY" } else { "SELL" },
        &mint[..8.min(mint.len())],
        sol_amount
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::PumpFun,
        signature: String::new(),
        tx_index: 0,
    })
}

// ---------------------------------------------------------------------------
// Raydium Launchpad Parser
// ---------------------------------------------------------------------------

/// Raydium Launchpad — 146 bytes
fn parse_raydium_146(
    buffer: &[u8],
    wallet: &str,
    txn: &SubscribeUpdateTransaction,
) -> Option<ParsedTrade> {
    let mint = extract_token_mint(txn);
    if mint.is_empty() {
        return None;
    }

    let virtual_base_reserve = parse_u64(buffer, 56)?;
    let virtual_quote_reserve = parse_u64(buffer, 64)?;
    let real_base_before = parse_u64(buffer, 72)?;
    let real_quote_before = parse_u64(buffer, 80)?;
    let real_base_after = parse_u64(buffer, 88)?;
    let real_quote_after = parse_u64(buffer, 96)?;

    let trade_direction = parse_u8(buffer, 144)? == 1;
    let is_buy = !trade_direction; // 0=buy, 1=sell

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sol_change = (real_quote_after as f64 - real_quote_before as f64).abs()
        / 1_000_000_000.0;
    let token_change = (real_base_after as f64 - real_base_before as f64).abs()
        / 1_000_000_000.0;

    let denominator = virtual_base_reserve as f64 - real_base_after as f64;
    let price = if denominator > 0.0 {
        let p = ((virtual_quote_reserve as f64 + real_quote_after as f64) / denominator)
            * 1_000_000_000.0;
        p as u64
    } else {
        0
    };

    trace!(
        "Raydium-146 {} {} — {:.4} SOL",
        if is_buy { "BUY" } else { "SELL" },
        &mint[..8.min(mint.len())],
        sol_change
    );

    Some(ParsedTrade {
        wallet: wallet.to_string(),
        token_mint: mint,
        is_buy,
        sol_amount: sol_change,
        token_amount: token_change,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::RaydiumLaunchpad,
        signature: String::new(),
        tx_index: 0,
    })
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Search for a valid unix timestamp in a buffer.
/// Scans common offsets for a u64 that looks like a recent timestamp.
fn find_timestamp_in_buffer(buffer: &[u8]) -> Option<u64> {
    let now = current_unix_timestamp();
    let min_ts = now.saturating_sub(86400 * 2); // 2 days ago
    let max_ts = now.saturating_add(3600); // 1 hour from now

    // Check common offsets where timestamps are found
    let offsets = [96, 16, 104, 112, 176, 88, 80];
    for &off in &offsets {
        if let Some(val) = parse_u64(buffer, off) {
            if val >= min_ts && val <= max_ts {
                return Some(val);
            }
        }
    }

    None
}

/// Get the current unix timestamp.
fn current_unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Batch-parse all inner-instruction data buffers from a transaction
// ---------------------------------------------------------------------------

/// Extract all DEX swap events from a single Yellowstone transaction update.
///
/// Walks the inner-instructions looking for recognizable event buffers and
/// returns all successfully parsed trades.
///
/// For Raydium AMM V4 transactions, uses the balance-diff method instead
/// of inner-instruction buffer parsing (per quant strategist directive).
pub fn parse_all_events(txn: &SubscribeUpdateTransaction) -> Vec<ParsedTrade> {
    let mut trades = Vec::new();

    let tx_inner = match txn.transaction.as_ref() {
        Some(t) => t,
        None => return trades,
    };
    let meta = match tx_inner.meta.as_ref() {
        Some(m) => m,
        None => return trades,
    };

    // Get slot and signature from the outer envelope
    let slot = txn.slot;
    let signature = tx_inner
        .transaction
        .as_ref()
        .and_then(|t| t.signatures.first())
        .map(|s| bs58::encode(s).into_string())
        .unwrap_or_default();

    // --- DIAGNOSTIC: log what we're seeing ---
    let total_inner_ixs: usize = meta.inner_instructions.iter()
        .map(|ii| ii.instructions.len())
        .sum();

    if total_inner_ixs > 0 {
        trace!(
            "DIAG slot={} sig={}.. inner_ix_groups={} total_inner_ixs={}",
            slot,
            &signature[..8.min(signature.len())],
            meta.inner_instructions.len(),
            total_inner_ixs,
        );
    }

    // --- Check if this is a Raydium AMM V4 transaction ---
    // If any account in the transaction is the Raydium V4 program,
    // use the balance-diff parser instead of inner-instruction parsing.
    let is_raydium_v4 = is_program_involved(txn, RAYDIUM_AMM_V4_PROGRAM);
    if is_raydium_v4 {
        if let Some(mut trade) = parse_raydium_v4_balance_diff(txn) {
            trade.slot = slot;
            trade.signature = signature.clone();
            trade.tx_index = 0;
            trades.push(trade);
        }
        // Still check inner instructions for any embedded PumpSwap events
        // in case of multi-hop swaps (Raydium → PumpSwap in one tx)
    }

    // Walk inner instructions looking for recognizable event buffers
    for (ix_idx, inner_ixs) in meta.inner_instructions.iter().enumerate() {
        for inner_ix in &inner_ixs.instructions {
            let buf_len = inner_ix.data.len();

            if buf_len < 16 {
                continue;
            }

            if let Some(mut trade) = parse_event_buffer(txn, &inner_ix.data) {
                trade.slot = slot;
                trade.signature = signature.clone();
                trade.tx_index = ix_idx as u32;
                trades.push(trade);
            }
        }
    }

    trades
}

// ---------------------------------------------------------------------------
// Raydium AMM V4 — Balance-Diff Parser
// ---------------------------------------------------------------------------
//
// Per quant strategist directive: "Do not attempt to parse the raw instruction
// buffer for Raydium swaps; instead, calculate sol_amount and token received
// by parsing preTokenBalances and postTokenBalances in the transaction
// metadata. This is computationally cheaper and IDL-agnostic."
//
// Algorithm:
//   1. Find the signer (fee payer) from account_keys[0]
//   2. Find the signer's account index in the transaction
//   3. For each mint in postTokenBalances that belongs to the signer:
//      - Compute delta = post_amount - pre_amount
//      - Identify WSOL leg (SOL side) and non-WSOL leg (token side)
//   4. If WSOL delta < 0 and token delta > 0 → BUY
//      If WSOL delta > 0 and token delta < 0 → SELL

/// Parse a Raydium AMM V4 swap using preTokenBalances / postTokenBalances diffs.
fn parse_raydium_v4_balance_diff(txn: &SubscribeUpdateTransaction) -> Option<ParsedTrade> {
    let wallet = extract_signer(txn)?;
    let tx_inner = txn.transaction.as_ref()?;
    let meta = tx_inner.meta.as_ref()?;
    let message = tx_inner.transaction.as_ref()?.message.as_ref()?;

    // Find the signer's account index (usually 0, but let's be precise)
    let signer_key = &message.account_keys.first()?;
    let signer_b58 = bs58::encode(signer_key).into_string();

    // Build a map of account_index → owner for token balances
    // We need to find which token balance entries belong to the signer
    // Token balances reference accounts by index, and the owner field tells us
    // who controls that token account.

    // Compute balance deltas per mint for the signer
    let mut sol_delta: f64 = 0.0;
    let mut token_delta: f64 = 0.0;
    let mut token_mint = String::new();
    let mut found_sol = false;
    let mut found_token = false;

    // Walk post_token_balances and match against pre_token_balances
    for post_bal in &meta.post_token_balances {
        // Only look at balances owned by the signer
        if post_bal.owner != signer_b58 {
            continue;
        }

        let mint = &post_bal.mint;
        let post_amount = post_bal
            .ui_token_amount
            .as_ref()
            .and_then(|a| a.ui_amount_string.parse::<f64>().ok())
            .unwrap_or(0.0);

        // Find matching pre_token_balance
        let pre_amount = meta
            .pre_token_balances
            .iter()
            .find(|pre| pre.account_index == post_bal.account_index && pre.mint == *mint)
            .and_then(|pre| {
                pre.ui_token_amount
                    .as_ref()
                    .and_then(|a| a.ui_amount_string.parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        let delta = post_amount - pre_amount;

        if *mint == WSOL_MINT {
            sol_delta = delta;
            found_sol = true;
        } else if delta.abs() > 0.0 {
            // This is the token leg (non-WSOL)
            token_delta = delta;
            token_mint = mint.clone();
            found_token = true;
        }
    }

    // If we didn't find both legs from owned accounts, try checking
    // SOL balance change from lamport pre/post balances
    if !found_sol {
        // Fall back to native SOL balance diff (pre_balances vs post_balances)
        if !meta.pre_balances.is_empty() && !meta.post_balances.is_empty() {
            // Signer is always index 0
            let pre_sol = meta.pre_balances.first().copied().unwrap_or(0) as f64 / 1e9;
            let post_sol = meta.post_balances.first().copied().unwrap_or(0) as f64 / 1e9;
            sol_delta = post_sol - pre_sol;
            found_sol = true;
        }
    }

    if !found_sol || !found_token || token_mint.is_empty() {
        // Can't determine both legs → skip
        trace!(
            "Raydium V4: could not determine both legs (sol={}, token={})",
            found_sol,
            found_token
        );
        return None;
    }

    // Determine direction:
    //   SOL decreased + token increased → BUY (spent SOL to get tokens)
    //   SOL increased + token decreased → SELL (sold tokens for SOL)
    let is_buy = sol_delta < 0.0 && token_delta > 0.0;
    let is_sell = sol_delta > 0.0 && token_delta < 0.0;

    if !is_buy && !is_sell {
        // Ambiguous or zero-delta swap (possibly a failed/dust transaction)
        trace!(
            "Raydium V4: ambiguous direction (sol_delta={:.6}, token_delta={:.6})",
            sol_delta,
            token_delta
        );
        return None;
    }

    let sol_amount = sol_delta.abs();
    let token_amount = token_delta.abs();

    // Sanity check: reject dust trades and unrealistic amounts
    if sol_amount < 0.0001 || sol_amount > 50_000.0 {
        return None;
    }

    // Compute price as lamports-per-token (scaled ×1e9)
    let price = if token_amount > 0.0 {
        ((sol_amount / token_amount) * 1e9) as u64
    } else {
        0
    };

    let timestamp = current_unix_timestamp();

    trace!(
        "RaydiumV4 {} {} — {:.6} SOL | {:.2} tokens",
        if is_buy { "BUY" } else { "SELL" },
        &token_mint[..8.min(token_mint.len())],
        sol_amount,
        token_amount
    );

    Some(ParsedTrade {
        wallet,
        token_mint,
        is_buy,
        sol_amount,
        token_amount,
        price,
        slot: 0,
        timestamp,
        dex_type: DexType::RaydiumAmmV4,
        signature: String::new(),
        tx_index: 0,
    })
}

/// Check if a specific program ID is involved in this transaction.
fn is_program_involved(txn: &SubscribeUpdateTransaction, program_id: &str) -> bool {
    let tx_inner = match txn.transaction.as_ref() {
        Some(t) => t,
        None => return false,
    };
    let message = match tx_inner.transaction.as_ref().and_then(|t| t.message.as_ref()) {
        Some(m) => m,
        None => return false,
    };

    let program_bytes = match bs58::decode(program_id).into_vec() {
        Ok(b) => b,
        Err(_) => return false,
    };

    message.account_keys.iter().any(|key| *key == program_bytes)
}
