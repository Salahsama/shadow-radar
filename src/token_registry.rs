// shadow-radar/src/token_registry.rs
//
// Tracks the T₀ slot (first observed liquidity initialization) for each
// token mint seen in the live stream.
//
// CRITICAL DESIGN (from quant strategist review):
//   T₀ is NOT immutable upon first observation. If the daemon later observes
//   a transaction for a mint that occurred in a LOWER slot than the currently
//   cached T₀, it MUST update T₀ and flag all dependent slot-delta
//   calculations for recalculation.
//
// This happens in practice because:
//   - gRPC replay may deliver out-of-order transactions during initial catch-up
//   - Network partitions can cause slot ordering anomalies
//   - The first observed tx is not always the actual pool init tx
//
// Implementation: moka async cache with 24h TTL, but with a custom
// update-on-lower-slot protocol.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// TokenOrigin — metadata about a token's first liquidity event
// ---------------------------------------------------------------------------

/// Metadata about when and how a token's liquidity was first initialized.
#[derive(Clone, Debug)]
pub struct TokenOrigin {
    /// Token mint address (base58).
    pub mint: String,
    /// The slot of the earliest observed transaction for this token.
    /// This is our best estimate of T₀ and may be revised downward.
    pub t0_slot: u64,
    /// Unix timestamp of the T₀ transaction (if available).
    pub t0_timestamp: u64,
    /// Signature of the T₀ transaction (for audit/debugging).
    pub first_seen_signature: String,
    /// How many times T₀ has been revised downward for this token.
    /// A high revision count suggests gRPC replay is still catching up.
    pub revision_count: u32,
}

// ---------------------------------------------------------------------------
// T₀ Update Result — returned when recording an observation
// ---------------------------------------------------------------------------

/// Result of recording a new observation for a token in the registry.
#[derive(Debug)]
pub enum T0UpdateResult {
    /// First time we've seen this token — T₀ set.
    NewToken {
        mint: String,
        t0_slot: u64,
    },
    /// We observed a transaction at a slot LOWER than the cached T₀.
    /// The caller must re-annotate all trades for this token.
    T0Revised {
        mint: String,
        old_t0_slot: u64,
        new_t0_slot: u64,
    },
    /// The observation was at a slot >= cached T₀. No change needed.
    Unchanged,
}

// ---------------------------------------------------------------------------
// TokenRegistry — the main T₀ tracking cache
// ---------------------------------------------------------------------------

/// Thread-safe, TTL-evicting registry of token liquidity origins.
///
/// Uses `moka::future::Cache` for automatic 24h eviction of stale tokens
/// and bounded memory usage. Wrapped entries use `Arc<RwLock<TokenOrigin>>`
/// to support the update-on-lower-slot protocol without cache churn.
pub struct TokenRegistry {
    /// The moka cache: `token_mint (String) → Arc<RwLock<TokenOrigin>>`
    inner: moka::future::Cache<String, Arc<RwLock<TokenOrigin>>>,
}

impl TokenRegistry {
    /// Create a new TokenRegistry with the specified TTL and capacity.
    ///
    /// # Parameters
    /// - `max_capacity`: Maximum number of tokens to track (e.g., 200_000).
    /// - `ttl_hours`: Time-to-live in hours for each entry (e.g., 24).
    pub fn new(max_capacity: u64, ttl_hours: u64) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(std::time::Duration::from_secs(ttl_hours * 3600))
            .build();

        Self { inner: cache }
    }

    /// Record an observation of a token at a given slot.
    ///
    /// If this is the first observation, sets T₀.
    /// If the slot is LOWER than the current T₀, revises T₀ downward
    /// and returns `T0Revised` so the caller can trigger re-annotation.
    ///
    /// This method is the core of the T₀ resilience protocol.
    pub async fn record_observation(
        &self,
        mint: &str,
        slot: u64,
        timestamp: u64,
        signature: &str,
    ) -> T0UpdateResult {
        // Fast path: check if token already exists
        if let Some(origin_lock) = self.inner.get(mint).await {
            let mut origin = origin_lock.write().await;

            if slot < origin.t0_slot {
                // !! T₀ revision — this slot is earlier than what we had
                let old_slot = origin.t0_slot;
                origin.t0_slot = slot;
                origin.t0_timestamp = timestamp;
                origin.first_seen_signature = signature.to_string();
                origin.revision_count = origin.revision_count.saturating_add(1);

                warn!(
                    "⚠️  T₀ REVISED for {} — old: slot {} → new: slot {} (revision #{})",
                    &mint[..8.min(mint.len())],
                    old_slot,
                    slot,
                    origin.revision_count
                );

                return T0UpdateResult::T0Revised {
                    mint: mint.to_string(),
                    old_t0_slot: old_slot,
                    new_t0_slot: slot,
                };
            }

            // Slot >= cached T₀ — no change
            return T0UpdateResult::Unchanged;
        }

        // Slow path: new token — insert with T₀ = this slot
        let origin = TokenOrigin {
            mint: mint.to_string(),
            t0_slot: slot,
            t0_timestamp: timestamp,
            first_seen_signature: signature.to_string(),
            revision_count: 0,
        };

        let origin_lock = Arc::new(RwLock::new(origin));
        self.inner.insert(mint.to_string(), origin_lock).await;

        debug!(
            "📍 T₀ recorded for {} at slot {}",
            &mint[..8.min(mint.len())],
            slot
        );

        T0UpdateResult::NewToken {
            mint: mint.to_string(),
            t0_slot: slot,
        }
    }

    /// Look up the T₀ slot for a given token mint.
    /// Returns `None` if the token has never been observed or has been evicted.
    pub async fn get_t0_slot(&self, mint: &str) -> Option<u64> {
        let origin_lock = self.inner.get(mint).await?;
        let origin = origin_lock.read().await;
        Some(origin.t0_slot)
    }

    /// Look up full origin metadata for a token.
    pub async fn get_origin(&self, mint: &str) -> Option<TokenOrigin> {
        let origin_lock = self.inner.get(mint).await?;
        let origin = origin_lock.read().await;
        Some(origin.clone())
    }

    /// Returns the number of tokens currently tracked.
    pub fn token_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Trigger cache maintenance (eviction of expired entries).
    /// Should be called periodically (e.g., every 5 minutes).
    pub async fn run_maintenance(&self) {
        self.inner.run_pending_tasks().await;
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_token_records_t0() {
        let registry = TokenRegistry::new(1000, 24);

        let result = registry
            .record_observation("mint_abc", 100, 1700000000, "sig_1")
            .await;

        match result {
            T0UpdateResult::NewToken { mint, t0_slot } => {
                assert_eq!(mint, "mint_abc");
                assert_eq!(t0_slot, 100);
            }
            _ => panic!("Expected NewToken"),
        }

        assert_eq!(registry.get_t0_slot("mint_abc").await, Some(100));
    }

    #[tokio::test]
    async fn test_same_or_higher_slot_unchanged() {
        let registry = TokenRegistry::new(1000, 24);

        registry
            .record_observation("mint_abc", 100, 1700000000, "sig_1")
            .await;

        // Same slot
        let result = registry
            .record_observation("mint_abc", 100, 1700000001, "sig_2")
            .await;
        assert!(matches!(result, T0UpdateResult::Unchanged));

        // Higher slot
        let result = registry
            .record_observation("mint_abc", 150, 1700000010, "sig_3")
            .await;
        assert!(matches!(result, T0UpdateResult::Unchanged));

        // T₀ should still be 100
        assert_eq!(registry.get_t0_slot("mint_abc").await, Some(100));
    }

    #[tokio::test]
    async fn test_lower_slot_revises_t0() {
        let registry = TokenRegistry::new(1000, 24);

        registry
            .record_observation("mint_abc", 100, 1700000000, "sig_1")
            .await;

        // Lower slot → T₀ revision
        let result = registry
            .record_observation("mint_abc", 95, 1699999990, "sig_0")
            .await;

        match result {
            T0UpdateResult::T0Revised {
                mint,
                old_t0_slot,
                new_t0_slot,
            } => {
                assert_eq!(mint, "mint_abc");
                assert_eq!(old_t0_slot, 100);
                assert_eq!(new_t0_slot, 95);
            }
            _ => panic!("Expected T0Revised"),
        }

        // T₀ should now be 95
        assert_eq!(registry.get_t0_slot("mint_abc").await, Some(95));
    }

    #[tokio::test]
    async fn test_revision_count_increments() {
        let registry = TokenRegistry::new(1000, 24);

        registry
            .record_observation("mint_abc", 100, 1700000000, "sig_1")
            .await;
        registry
            .record_observation("mint_abc", 95, 1699999990, "sig_0")
            .await;
        registry
            .record_observation("mint_abc", 90, 1699999980, "sig_minus1")
            .await;

        let origin = registry.get_origin("mint_abc").await.unwrap();
        assert_eq!(origin.t0_slot, 90);
        assert_eq!(origin.revision_count, 2);
    }

    #[tokio::test]
    async fn test_unknown_token_returns_none() {
        let registry = TokenRegistry::new(1000, 24);
        assert_eq!(registry.get_t0_slot("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_token_count() {
        let registry = TokenRegistry::new(1000, 24);

        registry
            .record_observation("mint_1", 100, 1700000000, "sig_1")
            .await;
        registry
            .record_observation("mint_2", 200, 1700000000, "sig_2")
            .await;
        registry
            .record_observation("mint_3", 300, 1700000000, "sig_3")
            .await;

        // moka may not update entry_count synchronously; run pending tasks
        registry.run_maintenance().await;
        assert!(registry.token_count() >= 3);
    }
}
