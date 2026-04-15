// shadow-radar/src/db.rs
//
// SQLite persistence layer for the live insider discovery engine.
// Stores qualified wallet targets and recent trades for consumption
// by the downstream execution bot.
//
// CRITICAL (per quant strategist):
//   - PRAGMA journal_mode=WAL    → enables concurrent read/write
//   - PRAGMA synchronous=NORMAL  → safe durability with WAL mode
//   This allows the execution bot to read `targets.db` while the
//   daemon writes to it, with zero database locks.
//
// This module is intentionally synchronous (rusqlite, not tokio-rusqlite).
// SQLite writes are fast enough that blocking the async runtime briefly
// is acceptable. For the hot path, writes are batched.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// TargetDb — the SQLite interface
// ---------------------------------------------------------------------------

/// SQLite database for storing qualified wallet targets.
///
/// Schema:
///   - `qualified_wallets`: Currently qualified wallets with metrics
///   - `wallet_trades`: Recent trade history for qualified wallets
///   - `discovery_log`: Log of when wallets were discovered/demoted
pub struct TargetDb {
    conn: Connection,
}

impl TargetDb {
    /// Open (or create) the target database at the given path.
    ///
    /// Initializes WAL mode, NORMAL synchronous, and creates all tables.
    pub fn open(db_path: &str) -> Result<Self> {
        let conn = Connection::open(Path::new(db_path))
            .context(format!("Failed to open SQLite database at {}", db_path))?;

        // --- Critical PRAGMAs for concurrent access ---
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA cache_size=-8000;",
        )
        .context("Failed to set SQLite PRAGMAs")?;

        let db = Self { conn };
        db.create_tables()?;

        info!("🗄️  SQLite target database opened: {} (WAL mode)", db_path);
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory SQLite database")?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .context("Failed to set SQLite PRAGMAs")?;

        let db = Self { conn };
        db.create_tables()?;

        Ok(db)
    }

    /// Create all tables if they don't exist.
    fn create_tables(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            -- Qualified wallets: the primary output consumed by the execution bot.
            -- The bot polls this table for `is_active = 1` wallets.
            CREATE TABLE IF NOT EXISTS qualified_wallets (
                address           TEXT PRIMARY KEY,
                win_rate          REAL NOT NULL,
                avg_entry_sol     REAL NOT NULL,
                avg_sell_tranches REAL NOT NULL,
                insider_window_ratio REAL NOT NULL,
                composite_score   REAL NOT NULL,
                total_cycles      INTEGER NOT NULL,
                total_trades      INTEGER NOT NULL,
                last_updated      INTEGER NOT NULL,
                qualified_at      INTEGER NOT NULL,
                is_active         INTEGER NOT NULL DEFAULT 1
            );

            -- Recent trades for qualified wallets (for audit / debugging).
            -- Keeps the last N trades per wallet. The daemon manages rotation.
            CREATE TABLE IF NOT EXISTS wallet_trades (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                wallet        TEXT NOT NULL,
                token_mint    TEXT NOT NULL,
                is_buy        INTEGER NOT NULL,
                sol_amount    REAL NOT NULL,
                token_amount  REAL NOT NULL,
                slot          INTEGER NOT NULL,
                timestamp     INTEGER NOT NULL,
                slot_delta_from_t0 INTEGER,
                is_sniper     INTEGER NOT NULL DEFAULT 0,
                is_insider    INTEGER NOT NULL DEFAULT 0,
                is_retail     INTEGER NOT NULL DEFAULT 0,
                signature     TEXT,
                dex_type      TEXT NOT NULL DEFAULT 'Unknown'
            );

            -- Discovery log: tracks when wallets are qualified/demoted.
            -- Useful for post-hoc analysis of the filter pipeline.
            CREATE TABLE IF NOT EXISTS discovery_log (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                wallet        TEXT NOT NULL,
                event_type    TEXT NOT NULL,  -- 'qualified', 'demoted', 'updated'
                reason        TEXT,
                composite_score REAL,
                timestamp     INTEGER NOT NULL
            );

            -- Index for fast execution-bot queries
            CREATE INDEX IF NOT EXISTS idx_qualified_active
                ON qualified_wallets(is_active) WHERE is_active = 1;

            -- Index for trade lookups by wallet
            CREATE INDEX IF NOT EXISTS idx_trades_wallet
                ON wallet_trades(wallet);

            -- Index for discovery log by wallet
            CREATE INDEX IF NOT EXISTS idx_discovery_wallet
                ON discovery_log(wallet);
            ",
            )
            .context("Failed to create database tables")?;

        debug!("📦 Database tables initialized");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Qualified wallet operations
    // -----------------------------------------------------------------------

    /// Upsert a qualified wallet into the database.
    ///
    /// If the wallet already exists, its metrics are updated.
    /// If it's new, it's inserted with `qualified_at = now`.
    pub fn upsert_qualified_wallet(&self, wallet: &QualifiedWalletRow) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO qualified_wallets
                    (address, win_rate, avg_entry_sol, avg_sell_tranches,
                     insider_window_ratio, composite_score, total_cycles,
                     total_trades, last_updated, qualified_at, is_active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)
                 ON CONFLICT(address) DO UPDATE SET
                    win_rate = excluded.win_rate,
                    avg_entry_sol = excluded.avg_entry_sol,
                    avg_sell_tranches = excluded.avg_sell_tranches,
                    insider_window_ratio = excluded.insider_window_ratio,
                    composite_score = excluded.composite_score,
                    total_cycles = excluded.total_cycles,
                    total_trades = excluded.total_trades,
                    last_updated = excluded.last_updated,
                    is_active = 1",
                params![
                    wallet.address,
                    wallet.win_rate,
                    wallet.avg_entry_sol,
                    wallet.avg_sell_tranches,
                    wallet.insider_window_ratio,
                    wallet.composite_score,
                    wallet.total_cycles,
                    wallet.total_trades,
                    wallet.last_updated,
                    wallet.qualified_at,
                ],
            )
            .context("Failed to upsert qualified wallet")?;

        debug!(
            "💾 Upserted qualified wallet: {}",
            &wallet.address[..8.min(wallet.address.len())]
        );
        Ok(())
    }

    /// Demote a wallet (set `is_active = 0`) with a reason.
    ///
    /// Called when a wallet's metrics decay below thresholds during
    /// periodic re-evaluation.
    pub fn demote_wallet(&self, address: &str, reason: &str) -> Result<()> {
        let now = current_unix_timestamp();

        self.conn
            .execute(
                "UPDATE qualified_wallets SET is_active = 0, last_updated = ?1
                 WHERE address = ?2",
                params![now, address],
            )
            .context("Failed to demote wallet")?;

        self.conn
            .execute(
                "INSERT INTO discovery_log (wallet, event_type, reason, timestamp)
                 VALUES (?1, 'demoted', ?2, ?3)",
                params![address, reason, now],
            )
            .context("Failed to log demotion")?;

        debug!(
            "📉 Demoted wallet: {} — {}",
            &address[..8.min(address.len())],
            reason
        );
        Ok(())
    }

    /// Log a discovery event (qualification).
    pub fn log_qualification(
        &self,
        address: &str,
        composite_score: f64,
    ) -> Result<()> {
        let now = current_unix_timestamp();

        self.conn
            .execute(
                "INSERT INTO discovery_log
                    (wallet, event_type, reason, composite_score, timestamp)
                 VALUES (?1, 'qualified', 'Passed all 4 heuristic filters', ?2, ?3)",
                params![address, composite_score, now],
            )
            .context("Failed to log qualification")?;

        Ok(())
    }

    /// Get all currently active qualified wallets.
    ///
    /// This is the primary query the execution bot uses.
    pub fn get_active_wallets(&self) -> Result<Vec<QualifiedWalletRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT address, win_rate, avg_entry_sol, avg_sell_tranches,
                        insider_window_ratio, composite_score, total_cycles,
                        total_trades, last_updated, qualified_at
                 FROM qualified_wallets
                 WHERE is_active = 1
                 ORDER BY composite_score DESC",
            )
            .context("Failed to prepare active wallets query")?;

        let wallets = stmt
            .query_map([], |row| {
                Ok(QualifiedWalletRow {
                    address: row.get(0)?,
                    win_rate: row.get(1)?,
                    avg_entry_sol: row.get(2)?,
                    avg_sell_tranches: row.get(3)?,
                    insider_window_ratio: row.get(4)?,
                    composite_score: row.get(5)?,
                    total_cycles: row.get(6)?,
                    total_trades: row.get(7)?,
                    last_updated: row.get(8)?,
                    qualified_at: row.get(9)?,
                })
            })
            .context("Failed to execute active wallets query")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to collect active wallets")?;

        Ok(wallets)
    }

    /// Get the count of active qualified wallets.
    pub fn active_wallet_count(&self) -> Result<u64> {
        let count: u64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM qualified_wallets WHERE is_active = 1",
                [],
                |row| row.get(0),
            )
            .context("Failed to count active wallets")?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // Trade logging operations
    // -----------------------------------------------------------------------

    /// Insert a trade record for audit purposes.
    pub fn insert_trade(&self, trade: &TradeRow) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO wallet_trades
                    (wallet, token_mint, is_buy, sol_amount, token_amount,
                     slot, timestamp, slot_delta_from_t0, is_sniper,
                     is_insider, is_retail, signature, dex_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    trade.wallet,
                    trade.token_mint,
                    trade.is_buy as i32,
                    trade.sol_amount,
                    trade.token_amount,
                    trade.slot,
                    trade.timestamp,
                    trade.slot_delta_from_t0,
                    trade.is_sniper as i32,
                    trade.is_insider as i32,
                    trade.is_retail as i32,
                    trade.signature,
                    trade.dex_type,
                ],
            )
            .context("Failed to insert trade")?;

        Ok(())
    }

    /// Batch-insert multiple trades in a single transaction.
    /// Much faster than individual inserts for bulk logging.
    pub fn insert_trades_batch(&mut self, trades: &[TradeRow]) -> Result<()> {
        let tx = self.conn.transaction()
            .context("Failed to begin batch insert transaction")?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO wallet_trades
                        (wallet, token_mint, is_buy, sol_amount, token_amount,
                         slot, timestamp, slot_delta_from_t0, is_sniper,
                         is_insider, is_retail, signature, dex_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )
                .context("Failed to prepare batch insert statement")?;

            for trade in trades {
                stmt.execute(params![
                    trade.wallet,
                    trade.token_mint,
                    trade.is_buy as i32,
                    trade.sol_amount,
                    trade.token_amount,
                    trade.slot,
                    trade.timestamp,
                    trade.slot_delta_from_t0,
                    trade.is_sniper as i32,
                    trade.is_insider as i32,
                    trade.is_retail as i32,
                    trade.signature,
                    trade.dex_type,
                ])
                .context("Failed to execute batch insert")?;
            }
        }

        tx.commit().context("Failed to commit batch insert")?;

        debug!("💾 Batch-inserted {} trades", trades.len());
        Ok(())
    }

    /// Prune old trade records beyond a retention window.
    /// Keeps the database from growing unbounded.
    pub fn prune_old_trades(&self, retention_hours: u64) -> Result<u64> {
        let cutoff = current_unix_timestamp().saturating_sub(retention_hours * 3600);

        let deleted = self
            .conn
            .execute(
                "DELETE FROM wallet_trades WHERE timestamp < ?1",
                params![cutoff],
            )
            .context("Failed to prune old trades")? as u64;

        if deleted > 0 {
            debug!("🗑️  Pruned {} trade records older than {}h", deleted, retention_hours);
        }

        Ok(deleted)
    }

    /// Demote wallets that haven't been updated recently.
    /// Called periodically to remove stale targets.
    pub fn demote_stale_wallets(&self, max_stale_secs: u64) -> Result<u64> {
        let cutoff = current_unix_timestamp().saturating_sub(max_stale_secs);

        let count = self
            .conn
            .execute(
                "UPDATE qualified_wallets
                 SET is_active = 0
                 WHERE is_active = 1 AND last_updated < ?1",
                params![cutoff],
            )
            .context("Failed to demote stale wallets")? as u64;

        if count > 0 {
            info!("📉 Demoted {} stale wallets (not updated in {}s)", count, max_stale_secs);
        }

        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Row types (plain structs for DB I/O)
// ---------------------------------------------------------------------------

/// A row in the `qualified_wallets` table.
#[derive(Clone, Debug)]
pub struct QualifiedWalletRow {
    pub address: String,
    pub win_rate: f64,
    pub avg_entry_sol: f64,
    pub avg_sell_tranches: f64,
    pub insider_window_ratio: f64,
    pub composite_score: f64,
    pub total_cycles: u32,
    pub total_trades: u64,
    pub last_updated: u64,
    pub qualified_at: u64,
}

/// A row in the `wallet_trades` table.
#[derive(Clone, Debug)]
pub struct TradeRow {
    pub wallet: String,
    pub token_mint: String,
    pub is_buy: bool,
    pub sol_amount: f64,
    pub token_amount: f64,
    pub slot: u64,
    pub timestamp: u64,
    pub slot_delta_from_t0: Option<u64>,
    pub is_sniper: bool,
    pub is_insider: bool,
    pub is_retail: bool,
    pub signature: String,
    pub dex_type: String,
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn current_unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wallet_row(addr: &str, win_rate: f64, score: f64) -> QualifiedWalletRow {
        QualifiedWalletRow {
            address: addr.to_string(),
            win_rate,
            avg_entry_sol: 1.5,
            avg_sell_tranches: 2.5,
            insider_window_ratio: 0.65,
            composite_score: score,
            total_cycles: 15,
            total_trades: 30,
            last_updated: current_unix_timestamp(),
            qualified_at: current_unix_timestamp(),
        }
    }

    fn make_trade_row(wallet: &str, mint: &str, is_buy: bool) -> TradeRow {
        TradeRow {
            wallet: wallet.to_string(),
            token_mint: mint.to_string(),
            is_buy,
            sol_amount: 1.5,
            token_amount: 1500.0,
            slot: 12345678,
            timestamp: current_unix_timestamp(),
            slot_delta_from_t0: Some(8),
            is_sniper: false,
            is_insider: true,
            is_retail: false,
            signature: "sig_test_123".to_string(),
            dex_type: "PumpSwap".to_string(),
        }
    }

    #[test]
    fn test_open_in_memory() {
        let db = TargetDb::open_in_memory().unwrap();
        assert_eq!(db.active_wallet_count().unwrap(), 0);
    }

    #[test]
    fn test_upsert_and_query_wallet() {
        let db = TargetDb::open_in_memory().unwrap();

        let wallet = make_wallet_row("wallet_abc123", 0.85, 4.5);
        db.upsert_qualified_wallet(&wallet).unwrap();

        let active = db.get_active_wallets().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].address, "wallet_abc123");
        assert!((active[0].win_rate - 0.85).abs() < 0.001);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let db = TargetDb::open_in_memory().unwrap();

        let mut wallet = make_wallet_row("wallet_abc", 0.80, 3.0);
        db.upsert_qualified_wallet(&wallet).unwrap();

        // Update same wallet with higher win rate
        wallet.win_rate = 0.90;
        wallet.composite_score = 5.0;
        db.upsert_qualified_wallet(&wallet).unwrap();

        let active = db.get_active_wallets().unwrap();
        assert_eq!(active.len(), 1); // still 1 row, not 2
        assert!((active[0].win_rate - 0.90).abs() < 0.001);
        assert!((active[0].composite_score - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_demote_wallet() {
        let db = TargetDb::open_in_memory().unwrap();

        let wallet = make_wallet_row("wallet_to_demote", 0.85, 4.0);
        db.upsert_qualified_wallet(&wallet).unwrap();
        assert_eq!(db.active_wallet_count().unwrap(), 1);

        db.demote_wallet("wallet_to_demote", "Win rate decayed below 80%")
            .unwrap();
        assert_eq!(db.active_wallet_count().unwrap(), 0);
    }

    #[test]
    fn test_insert_and_query_trades() {
        let db = TargetDb::open_in_memory().unwrap();

        let trade = make_trade_row("wallet_abc", "mint_xyz", true);
        db.insert_trade(&trade).unwrap();

        let count: u64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM wallet_trades WHERE wallet = ?1",
                params!["wallet_abc"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_batch_insert_trades() {
        let mut db = TargetDb::open_in_memory().unwrap();

        let trades: Vec<TradeRow> = (0..100)
            .map(|i| TradeRow {
                wallet: "wallet_batch".to_string(),
                token_mint: format!("mint_{}", i),
                is_buy: i % 2 == 0,
                sol_amount: 1.0 + (i as f64 * 0.01),
                token_amount: 1000.0,
                slot: 12345678 + i,
                timestamp: current_unix_timestamp(),
                slot_delta_from_t0: Some(10),
                is_sniper: false,
                is_insider: true,
                is_retail: false,
                signature: format!("sig_batch_{}", i),
                dex_type: "PumpSwap".to_string(),
            })
            .collect();

        db.insert_trades_batch(&trades).unwrap();

        let count: u64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM wallet_trades WHERE wallet = ?1",
                params!["wallet_batch"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 100);
    }

    #[test]
    fn test_multiple_wallets_ordered_by_score() {
        let db = TargetDb::open_in_memory().unwrap();

        db.upsert_qualified_wallet(&make_wallet_row("wallet_low", 0.80, 2.0))
            .unwrap();
        db.upsert_qualified_wallet(&make_wallet_row("wallet_high", 0.95, 8.0))
            .unwrap();
        db.upsert_qualified_wallet(&make_wallet_row("wallet_mid", 0.85, 5.0))
            .unwrap();

        let active = db.get_active_wallets().unwrap();
        assert_eq!(active.len(), 3);
        // Should be ordered by composite_score DESC
        assert_eq!(active[0].address, "wallet_high");
        assert_eq!(active[1].address, "wallet_mid");
        assert_eq!(active[2].address, "wallet_low");
    }

    #[test]
    fn test_log_qualification() {
        let db = TargetDb::open_in_memory().unwrap();

        db.log_qualification("wallet_abc", 4.5).unwrap();

        let count: u64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM discovery_log WHERE wallet = ?1 AND event_type = 'qualified'",
                params!["wallet_abc"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_prune_old_trades() {
        let db = TargetDb::open_in_memory().unwrap();

        // Insert a trade with a very old timestamp
        let old_trade = TradeRow {
            wallet: "wallet_old".to_string(),
            token_mint: "mint_old".to_string(),
            is_buy: true,
            sol_amount: 1.0,
            token_amount: 1000.0,
            slot: 1000,
            timestamp: 1000000, // very old
            slot_delta_from_t0: None,
            is_sniper: false,
            is_insider: false,
            is_retail: false,
            signature: "sig_old".to_string(),
            dex_type: "PumpFun".to_string(),
        };
        db.insert_trade(&old_trade).unwrap();

        // Insert a recent trade
        let recent_trade = make_trade_row("wallet_recent", "mint_recent", true);
        db.insert_trade(&recent_trade).unwrap();

        // Prune trades older than 24h — should remove the old one
        let pruned = db.prune_old_trades(24).unwrap();
        assert_eq!(pruned, 1);

        // Only recent trade should remain
        let count: u64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM wallet_trades", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
