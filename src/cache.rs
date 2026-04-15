// shadow-radar/src/cache.rs
//
// Central cache orchestrator for the live insider discovery engine.
// Manages three moka-backed caches:
//   1. Wallet behavior states (24h sliding window)
//   2. Token T₀ registry (24h TTL)
//   3. Per-token buy index (for anti-herd cross-wallet correlation)
//
// All caches are fully concurrent (moka is lock-free for reads) and
// bounded by both entry count and TTL to prevent unbounded memory growth.
//
// The orchestrator is the single point of truth for trade ingestion:
//   parse_all_events() → cache.process_trade() → heuristic evaluation
//
// Security: Uses Arc<RwLock<T>> for mutable cached values.
//           Read-heavy workload: RwLock is optimal (many readers, rare writers).

use crate::token_registry::{T0UpdateResult, TokenRegistry};
use crate::wallet_state::{AnnotatedTrade, WalletState};

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

// ---------------------------------------------------------------------------
// BuyEntry — an element in the per-token buy index for anti-herd detection
// ---------------------------------------------------------------------------

/// A single buy event in the per-token buy index.
/// Used by Filter 3 (anti-herd) to detect clusters of correlated buys
/// following a specific wallet's purchase.
#[derive(Clone, Debug)]
pub struct BuyEntry {
    /// Wallet that made this buy.
    pub wallet: String,
    /// Slot of the buy transaction.
    pub slot: u64,
    /// Transaction index within the slot (intra-block ordering).
    pub tx_index: u32,
    /// SOL amount of the buy.
    pub sol_amount: f64,
    /// Unix timestamp.
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// CacheConfig — tuneable parameters for the cache layer
// ---------------------------------------------------------------------------

/// Configuration for the InsiderCache.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Maximum number of wallets to track (memory bound).
    pub max_wallets: u64,
    /// Maximum number of tokens in the T₀ registry.
    pub max_tokens: u64,
    /// Maximum number of token buy-index entries.
    pub max_token_buy_indexes: u64,
    /// TTL for cache entries in hours.
    pub ttl_hours: u64,
    /// Anti-sniper: minimum slot delta from T₀ for the insider window.
    pub anti_sniper_min_slot_delta: u64,
    /// Anti-sniper: maximum slot delta from T₀ for the insider window.
    pub anti_sniper_max_slot_delta: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_wallets: 500_000,
            max_tokens: 200_000,
            max_token_buy_indexes: 200_000,
            ttl_hours: 24,
            anti_sniper_min_slot_delta: 3,
            anti_sniper_max_slot_delta: 15,
        }
    }
}

// ---------------------------------------------------------------------------
// InsiderCache — the main orchestrator
// ---------------------------------------------------------------------------

/// Central cache orchestrator for the live insider discovery engine.
///
/// Thread-safe, lock-free reads, bounded memory. All trade ingestion
/// flows through `process_trade()` which handles:
///   1. T₀ recording (with lower-slot revision)
///   2. Trade annotation (slot-delta enrichment)
///   3. Wallet state update (position tracking, cycle completion)
///   4. Per-token buy index update (for anti-herd analysis)
pub struct InsiderCache {
    /// Per-wallet behavior state — TTL 24h, max 500k entries.
    pub wallets: moka::future::Cache<String, Arc<RwLock<WalletState>>>,

    /// Token T₀ registry — TTL 24h, max 200k entries.
    pub token_registry: TokenRegistry,

    /// Per-token buy index — for anti-herd cross-wallet correlation.
    /// Key: token_mint, Value: sorted vec of (slot, wallet, tx_index).
    pub token_buys: moka::future::Cache<String, Arc<RwLock<Vec<BuyEntry>>>>,

    /// Cache configuration.
    config: CacheConfig,

    // --- Live stats ---
    /// Total trades processed since daemon start.
    pub trades_processed: Arc<std::sync::atomic::AtomicU64>,
    /// Total wallets currently tracked (approximate — moka is eventually consistent).
    pub wallets_tracked: Arc<std::sync::atomic::AtomicU64>,
    /// Total T₀ revisions observed.
    pub t0_revisions: Arc<std::sync::atomic::AtomicU64>,
}

impl InsiderCache {
    /// Create a new InsiderCache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        let wallet_cache = moka::future::Cache::builder()
            .max_capacity(config.max_wallets)
            .time_to_live(std::time::Duration::from_secs(config.ttl_hours * 3600))
            .build();

        let token_buys_cache = moka::future::Cache::builder()
            .max_capacity(config.max_token_buy_indexes)
            .time_to_live(std::time::Duration::from_secs(config.ttl_hours * 3600))
            .build();

        let token_registry = TokenRegistry::new(config.max_tokens, config.ttl_hours);

        Self {
            wallets: wallet_cache,
            token_registry,
            token_buys: token_buys_cache,
            config,
            trades_processed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wallets_tracked: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            t0_revisions: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Create a new InsiderCache with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Process a single parsed trade through the full pipeline.
    ///
    /// This is the **hot path** — called for every DEX trade in the live stream.
    /// The pipeline:
    ///   1. Record token observation in T₀ registry (may trigger revision)
    ///   2. Look up T₀ for annotation
    ///   3. Create AnnotatedTrade with slot-delta enrichment
    ///   4. Update wallet state (positions, cycles, metrics)
    ///   5. Index buy in per-token buy list (if it's a buy)
    ///   6. If T₀ was revised, re-annotate all affected wallets
    ///
    /// Returns the wallet address for downstream heuristic evaluation.
    pub async fn process_trade(
        &self,
        trade: crate::models::ParsedTrade,
    ) -> ProcessResult {
        let wallet_addr = trade.wallet.clone();
        let token_mint = trade.token_mint.clone();
        let slot = trade.slot;
        let timestamp = trade.timestamp;
        let signature = trade.signature.clone();
        let is_buy = trade.is_buy;
        let sol_amount = trade.sol_amount;
        let tx_index = trade.tx_index;

        self.trades_processed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // --- Step 1: Record token observation in T₀ registry ---
        let t0_result = self
            .token_registry
            .record_observation(&token_mint, slot, timestamp, &signature)
            .await;

        // --- Step 2: Look up T₀ for annotation ---
        let t0_slot = self.token_registry.get_t0_slot(&token_mint).await;

        // --- Step 3: Create AnnotatedTrade ---
        let annotated = AnnotatedTrade::annotate(
            trade,
            t0_slot,
            self.config.anti_sniper_min_slot_delta,
            self.config.anti_sniper_max_slot_delta,
        );

        // --- Step 4: Update wallet state ---
        let wallet_state = self.get_or_create_wallet(&wallet_addr).await;
        {
            let mut state = wallet_state.write().await;
            state.ingest_trade(annotated);
        }

        // --- Step 5: Index buy in per-token buy list ---
        if is_buy {
            self.index_buy(&token_mint, BuyEntry {
                wallet: wallet_addr.clone(),
                slot,
                tx_index,
                sol_amount,
                timestamp,
            })
            .await;
        }

        // --- Step 6: Handle T₀ revisions ---
        let mut t0_revised = false;
        if let T0UpdateResult::T0Revised {
            mint: ref revised_mint,
            new_t0_slot,
            ..
        } = t0_result
        {
            t0_revised = true;
            self.t0_revisions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // Re-annotate all wallets that have traded this token
            self.re_annotate_token_across_wallets(revised_mint, new_t0_slot)
                .await;
        }

        ProcessResult {
            wallet: wallet_addr,
            token_mint,
            t0_revised,
        }
    }

    /// Get or create a wallet state entry.
    async fn get_or_create_wallet(
        &self,
        wallet_addr: &str,
    ) -> Arc<RwLock<WalletState>> {
        if let Some(existing) = self.wallets.get(wallet_addr).await {
            return existing;
        }

        // New wallet — create and insert
        let state = Arc::new(RwLock::new(WalletState::new(wallet_addr.to_string())));
        self.wallets
            .insert(wallet_addr.to_string(), state.clone())
            .await;

        self.wallets_tracked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        trace!(
            "📋 New wallet tracked: {}",
            &wallet_addr[..8.min(wallet_addr.len())]
        );

        state
    }

    /// Index a buy transaction in the per-token buy list.
    /// Used by anti-herd filter to find correlated buy clusters.
    async fn index_buy(&self, token_mint: &str, entry: BuyEntry) {
        if let Some(list_lock) = self.token_buys.get(token_mint).await {
            let mut list = list_lock.write().await;
            // Insert sorted by (slot, tx_index) for efficient scanning
            let pos = list
                .binary_search_by(|e| e.slot.cmp(&entry.slot).then(e.tx_index.cmp(&entry.tx_index)))
                .unwrap_or_else(|p| p);
            list.insert(pos, entry);

            // Cap the list to prevent unbounded growth (keep last 1000 buys per token)
            if list.len() > 1000 {
                let excess = list.len() - 1000;
                list.drain(..excess);
            }
        } else {
            // New token buy list
            let list = Arc::new(RwLock::new(vec![entry]));
            self.token_buys
                .insert(token_mint.to_string(), list)
                .await;
        }
    }

    /// Re-annotate all wallets that have recent trades for the given token.
    /// Called when T₀ is revised downward for a token.
    ///
    /// This is the expensive path — but T₀ revisions should be rare in steady state.
    async fn re_annotate_token_across_wallets(
        &self,
        token_mint: &str,
        new_t0_slot: u64,
    ) {
        debug!(
            "🔄 Re-annotating all wallets for token {} with new T₀={}",
            &token_mint[..8.min(token_mint.len())],
            new_t0_slot
        );

        // We need to iterate through wallets that might have traded this token.
        // moka doesn't support iteration, so we track affected wallets through
        // the token buy index.
        if let Some(buy_list_lock) = self.token_buys.get(token_mint).await {
            let buy_list = buy_list_lock.read().await;
            let affected_wallets: Vec<String> = buy_list
                .iter()
                .map(|e| e.wallet.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            drop(buy_list); // Release read lock before acquiring write locks

            for wallet_addr in &affected_wallets {
                if let Some(wallet_lock) = self.wallets.get(wallet_addr).await {
                    let mut state = wallet_lock.write().await;
                    state.re_annotate_token(
                        token_mint,
                        new_t0_slot,
                        self.config.anti_sniper_min_slot_delta,
                        self.config.anti_sniper_max_slot_delta,
                    );
                }
            }

            debug!(
                "🔄 Re-annotated {} wallets for token {}",
                affected_wallets.len(),
                &token_mint[..8.min(token_mint.len())]
            );
        }
    }

    /// Get a read-only snapshot of a wallet's current state.
    /// Used by the heuristic evaluator.
    pub async fn get_wallet_state(&self, wallet_addr: &str) -> Option<WalletState> {
        let lock = self.wallets.get(wallet_addr).await?;
        let state = lock.read().await;
        Some(state.clone())
    }

    /// Get the buy entries for a token (for anti-herd analysis).
    /// Returns entries sorted by (slot, tx_index).
    pub async fn get_token_buys(&self, token_mint: &str) -> Vec<BuyEntry> {
        if let Some(lock) = self.token_buys.get(token_mint).await {
            let list = lock.read().await;
            list.clone()
        } else {
            Vec::new()
        }
    }

    /// Count copycats: for a given buy at (slot, tx_index) on a token,
    /// count distinct wallets that also bought the SAME token within
    /// `window_slots` slots, excluding the original wallet.
    ///
    /// This is the core primitive for anti-herd (Filter 3b).
    pub async fn count_copycats(
        &self,
        token_mint: &str,
        wallet: &str,
        buy_slot: u64,
        buy_tx_index: u32,
        window_slots: u64,
    ) -> u32 {
        let buys = self.get_token_buys(token_mint).await;

        let mut copycat_wallets = std::collections::HashSet::new();

        for entry in &buys {
            // Must be a different wallet
            if entry.wallet == wallet {
                continue;
            }

            let slot_delta = entry.slot.saturating_sub(buy_slot);

            if slot_delta == 0 {
                // Same block: only count if tx_index is AFTER the original buy
                if entry.tx_index > buy_tx_index {
                    copycat_wallets.insert(&entry.wallet);
                }
            } else if slot_delta <= window_slots {
                // Within the window (1..=window_slots blocks later)
                copycat_wallets.insert(&entry.wallet);
            }
            // slot_delta > window_slots → outside the window, skip
        }

        copycat_wallets.len() as u32
    }

    /// Run periodic maintenance on all caches.
    /// Should be called every ~5 minutes to:
    ///   - Evict expired entries from moka caches
    ///   - Log cache statistics
    pub async fn run_maintenance(&self) {
        self.wallets.run_pending_tasks().await;
        self.token_buys.run_pending_tasks().await;
        self.token_registry.run_maintenance().await;

        let trades = self
            .trades_processed
            .load(std::sync::atomic::Ordering::Relaxed);
        let wallets = self.wallets.entry_count();
        let tokens = self.token_registry.token_count();
        let revisions = self
            .t0_revisions
            .load(std::sync::atomic::Ordering::Relaxed);

        info!(
            "📊 Cache stats: {} trades processed | {} wallets | {} tokens | {} T₀ revisions",
            trades, wallets, tokens, revisions
        );
    }

    /// Get current cache statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            trades_processed: self
                .trades_processed
                .load(std::sync::atomic::Ordering::Relaxed),
            wallets_tracked: self.wallets.entry_count(),
            tokens_tracked: self.token_registry.token_count(),
            token_buy_indexes: self.token_buys.entry_count(),
            t0_revisions: self
                .t0_revisions
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// ProcessResult — returned from process_trade()
// ---------------------------------------------------------------------------

/// Result of processing a single trade through the cache pipeline.
pub struct ProcessResult {
    /// Wallet address that was updated.
    pub wallet: String,
    /// Token mint involved in the trade.
    pub token_mint: String,
    /// Whether T₀ was revised for this token (requires re-evaluation).
    pub t0_revised: bool,
}

// ---------------------------------------------------------------------------
// CacheStats — diagnostic snapshot
// ---------------------------------------------------------------------------

/// Diagnostic snapshot of cache state.
#[derive(Clone, Debug)]
pub struct CacheStats {
    pub trades_processed: u64,
    pub wallets_tracked: u64,
    pub tokens_tracked: u64,
    pub token_buy_indexes: u64,
    pub t0_revisions: u64,
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DexType, ParsedTrade};

    fn make_trade(
        wallet: &str,
        mint: &str,
        is_buy: bool,
        sol: f64,
        slot: u64,
        ts: u64,
    ) -> ParsedTrade {
        ParsedTrade {
            wallet: wallet.to_string(),
            token_mint: mint.to_string(),
            is_buy,
            sol_amount: sol,
            token_amount: sol * 1000.0,
            price: 1_000_000,
            slot,
            timestamp: ts,
            dex_type: DexType::PumpSwap,
            signature: format!("sig_{}_{}", wallet, slot),
            tx_index: 0,
        }
    }

    #[tokio::test]
    async fn test_process_trade_new_wallet() {
        let cache = InsiderCache::with_defaults();

        let trade = make_trade("wallet1", "token1", true, 1.0, 100, 1700000000);
        let result = cache.process_trade(trade).await;

        assert_eq!(result.wallet, "wallet1");
        assert!(!result.t0_revised);

        // Wallet should exist in cache
        let state = cache.get_wallet_state("wallet1").await.unwrap();
        assert_eq!(state.lifetime_trade_count, 1);
        assert_eq!(state.per_token_positions.len(), 1);
    }

    #[tokio::test]
    async fn test_process_trade_t0_recorded() {
        let cache = InsiderCache::with_defaults();

        let trade = make_trade("wallet1", "token1", true, 1.0, 100, 1700000000);
        cache.process_trade(trade).await;

        let t0 = cache.token_registry.get_t0_slot("token1").await;
        assert_eq!(t0, Some(100));
    }

    #[tokio::test]
    async fn test_process_trade_t0_revision_triggers_reannotation() {
        let cache = InsiderCache::with_defaults();

        // First trade at slot 100
        let trade1 = make_trade("wallet1", "token1", true, 1.0, 100, 1700000000);
        cache.process_trade(trade1).await;

        // Second trade at LOWER slot 90 → should trigger T₀ revision
        let trade2 = make_trade("wallet2", "token1", true, 0.5, 90, 1699999990);
        let result = cache.process_trade(trade2).await;

        assert!(result.t0_revised);
        assert_eq!(
            cache.token_registry.get_t0_slot("token1").await,
            Some(90)
        );
    }

    #[tokio::test]
    async fn test_buy_index_populated() {
        let cache = InsiderCache::with_defaults();

        let trade = make_trade("wallet1", "token1", true, 1.0, 100, 1700000000);
        cache.process_trade(trade).await;

        let buys = cache.get_token_buys("token1").await;
        assert_eq!(buys.len(), 1);
        assert_eq!(buys[0].wallet, "wallet1");
        assert_eq!(buys[0].slot, 100);
    }

    #[tokio::test]
    async fn test_sell_not_indexed_in_buys() {
        let cache = InsiderCache::with_defaults();

        let sell = make_trade("wallet1", "token1", false, 1.0, 100, 1700000000);
        cache.process_trade(sell).await;

        let buys = cache.get_token_buys("token1").await;
        assert_eq!(buys.len(), 0);
    }

    #[tokio::test]
    async fn test_count_copycats() {
        let cache = InsiderCache::with_defaults();

        // Wallet A buys at slot 100
        let trade_a = ParsedTrade {
            wallet: "walletA".to_string(),
            token_mint: "token1".to_string(),
            is_buy: true,
            sol_amount: 1.0,
            token_amount: 1000.0,
            price: 1_000_000,
            slot: 100,
            timestamp: 1700000000,
            dex_type: DexType::PumpSwap,
            signature: "sig_a".to_string(),
            tx_index: 0,
        };
        cache.process_trade(trade_a).await;

        // Wallets B, C, D buy the same token within 3 slots
        for (name, slot, tx_idx) in [("walletB", 100u64, 1u32), ("walletC", 101, 0), ("walletD", 103, 0)] {
            let t = ParsedTrade {
                wallet: name.to_string(),
                token_mint: "token1".to_string(),
                is_buy: true,
                sol_amount: 0.5,
                token_amount: 500.0,
                price: 1_000_000,
                slot,
                timestamp: 1700000000,
                dex_type: DexType::PumpSwap,
                signature: format!("sig_{}", name),
                tx_index: tx_idx,
            };
            cache.process_trade(t).await;
        }

        // Count copycats for walletA's buy (window = 3 slots)
        let copycats = cache
            .count_copycats("token1", "walletA", 100, 0, 3)
            .await;
        assert_eq!(copycats, 3); // B (same slot, higher tx_index), C (slot+1), D (slot+3)
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = InsiderCache::with_defaults();

        let trade = make_trade("wallet1", "token1", true, 1.0, 100, 1700000000);
        cache.process_trade(trade).await;

        // moka is eventually consistent — flush pending tasks before checking counts
        cache.run_maintenance().await;

        let stats = cache.stats();
        assert_eq!(stats.trades_processed, 1);
        assert!(stats.wallets_tracked >= 1);
        assert!(stats.tokens_tracked >= 1);
    }

    #[tokio::test]
    async fn test_full_cycle_through_cache() {
        let cache = InsiderCache::with_defaults();

        // Buy 2 SOL of token1
        let buy = make_trade("walletX", "token1", true, 2.0, 100, 1700000000);
        cache.process_trade(buy).await;

        // Sell 1 SOL (tranche 1)
        let sell1 = make_trade("walletX", "token1", false, 1.0, 110, 1700000010);
        cache.process_trade(sell1).await;

        // Sell 1.5 SOL (tranche 2 — closes position)
        let sell2 = make_trade("walletX", "token1", false, 1.5, 120, 1700000020);
        cache.process_trade(sell2).await;

        let state = cache.get_wallet_state("walletX").await.unwrap();
        assert_eq!(state.completed_cycles.len(), 1);
        assert_eq!(state.per_token_positions.len(), 0); // closed
        assert!(state.completed_cycles[0].is_win); // 2.5 > 2.0
        assert_eq!(state.completed_cycles[0].sell_tranche_count, 2);
    }
}
