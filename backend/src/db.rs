//! SQLite persistence for bar data.
//!
//! Live mode records bars to SQLite for later replay in backtests.
//! Uses `rusqlite` with the `bundled` feature so no system SQLite is required.

use crate::BarData;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

/// Thread-safe wrapper around a SQLite connection for bar persistence.
/// `Mutex<Connection>` makes it `Send + Sync` for `Arc` sharing.
pub struct BarDb {
    conn: Mutex<Connection>,
}

impl BarDb {
    /// Opens (or creates) the database at `path`, creating parent directories
    /// and the `bars` table if they don't exist.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bars (
                symbol    TEXT    NOT NULL,
                timestamp INTEGER NOT NULL,
                open      REAL    NOT NULL,
                high      REAL    NOT NULL,
                low       REAL    NOT NULL,
                close     REAL    NOT NULL,
                volume    REAL    NOT NULL,
                PRIMARY KEY (symbol, timestamp)
            )",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Inserts or replaces a single bar (handles IBKR in-progress bar updates).
    pub fn upsert_bar(&self, symbol: &str, bar: &BarData) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO bars (symbol, timestamp, open, high, low, close, volume)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                symbol,
                bar.timestamp as i64,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume
            ],
        )?;
        Ok(())
    }

    /// Batch upsert in a single transaction for efficiency.
    pub fn upsert_bars(&self, symbol: &str, bars: &[BarData]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for bar in bars {
            tx.execute(
                "INSERT OR REPLACE INTO bars (symbol, timestamp, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    symbol,
                    bar.timestamp as i64,
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume
                ],
            )?;
        }
        tx.commit()
    }

    /// Loads bars for a symbol within a timestamp range (inclusive), sorted ascending.
    pub fn load_bars(
        &self,
        symbol: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> rusqlite::Result<Vec<BarData>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT timestamp, open, high, low, close, volume
             FROM bars WHERE symbol = ?1 AND timestamp >= ?2 AND timestamp <= ?3
             ORDER BY timestamp ASC",
        )?;
        let rows = stmt.query_map(params![symbol, from_ms as i64, to_ms as i64], |row| {
            Ok(BarData {
                timestamp: row.get::<_, i64>(0)? as u64,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
                volume: row.get(5)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_bar(ts: u64) -> BarData {
        BarData {
            timestamp: ts,
            open: 10.0,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 100.0,
        }
    }

    #[test]
    fn create_and_read_bars() {
        let dir = TempDir::new().unwrap();
        let db = BarDb::open(dir.path().join("test.db")).unwrap();
        let bar = test_bar(1000);
        db.upsert_bar("TEST", &bar).unwrap();

        let bars = db.load_bars("TEST", 0, 2000).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].timestamp, 1000);
        assert_eq!(bars[0].close, 10.5);
    }

    #[test]
    fn upsert_replaces_same_timestamp() {
        let dir = TempDir::new().unwrap();
        let db = BarDb::open(dir.path().join("test.db")).unwrap();
        db.upsert_bar("TEST", &test_bar(1000)).unwrap();

        let mut updated = test_bar(1000);
        updated.close = 11.0;
        db.upsert_bar("TEST", &updated).unwrap();

        let bars = db.load_bars("TEST", 0, 2000).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].close, 11.0);
    }

    #[test]
    fn load_bars_filters_by_range() {
        let dir = TempDir::new().unwrap();
        let db = BarDb::open(dir.path().join("test.db")).unwrap();

        for ts in [1000, 2000, 3000, 4000, 5000] {
            db.upsert_bar("TEST", &test_bar(ts)).unwrap();
        }

        let bars = db.load_bars("TEST", 2000, 4000).unwrap();
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].timestamp, 2000);
        assert_eq!(bars[2].timestamp, 4000);
    }

    #[test]
    fn batch_upsert() {
        let dir = TempDir::new().unwrap();
        let db = BarDb::open(dir.path().join("test.db")).unwrap();

        let bars: Vec<BarData> = (1..=5).map(|i| test_bar(i * 1000)).collect();
        db.upsert_bars("TEST", &bars).unwrap();

        let loaded = db.load_bars("TEST", 0, 10000).unwrap();
        assert_eq!(loaded.len(), 5);
    }

    #[test]
    fn different_symbols_isolated() {
        let dir = TempDir::new().unwrap();
        let db = BarDb::open(dir.path().join("test.db")).unwrap();

        db.upsert_bar("AAA", &test_bar(1000)).unwrap();
        db.upsert_bar("BBB", &test_bar(1000)).unwrap();
        db.upsert_bar("BBB", &test_bar(2000)).unwrap();

        assert_eq!(db.load_bars("AAA", 0, 10000).unwrap().len(), 1);
        assert_eq!(db.load_bars("BBB", 0, 10000).unwrap().len(), 2);
        assert_eq!(db.load_bars("CCC", 0, 10000).unwrap().len(), 0);
    }
}
