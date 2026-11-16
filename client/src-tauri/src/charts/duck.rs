//! DuckDB handle for the `bars` table (Roadmap #23, hybrid: only bars live
//! in DuckDB; orders, accounts, journals and settings stay in SQLite).
//!
//! The `duckdb` crate is synchronous and one `Connection` is not shared
//! between threads, so the connection sits behind a mutex and every call runs
//! on tokio's blocking pool (`BarStore::run`). Writes are small batches from
//! the recorder flush (every few seconds), the Fyers backfill and the hourly
//! maintenance, so one connection is enough and also gives single-writer
//! behaviour for free.

use anyhow::{anyhow, Result};
use duckdb::{Config, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Column order matters: the migration appender writes rows in this order.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bars (
    instrument  VARCHAR NOT NULL,
    tf          VARCHAR NOT NULL CHECK (tf IN ('1m','1d')),
    ts          BIGINT  NOT NULL,
    open        DOUBLE  NOT NULL,
    high        DOUBLE  NOT NULL,
    low         DOUBLE  NOT NULL,
    close       DOUBLE  NOT NULL,
    volume      DOUBLE  NOT NULL DEFAULT 0,
    oi          DOUBLE,
    src         VARCHAR NOT NULL DEFAULT 'live',
    expiry_date VARCHAR,
    PRIMARY KEY (instrument, tf, ts)
);
-- Per-connection scratch table used by `store::upsert_live` to merge a batch.
CREATE TEMP TABLE IF NOT EXISTS bars_stage (
    instrument  VARCHAR,
    ts          BIGINT,
    open        DOUBLE,
    high        DOUBLE,
    low         DOUBLE,
    close       DOUBLE,
    volume      DOUBLE,
    oi          DOUBLE,
    expiry_date VARCHAR
);
"#;

#[derive(Clone)]
pub struct BarStore {
    conn: Arc<Mutex<Connection>>,
}

impl BarStore {
    /// Open (or create) the DuckDB file. Threads and memory are capped so the
    /// engine stays a good neighbour on a desktop.
    pub fn open(path: &Path) -> Result<Self> {
        let cfg = Config::default().with("threads", "2")?.with("memory_limit", "512MB")?;
        let conn = Connection::open_with_flags(path, cfg)?;
        Self::init(conn)
    }

    /// In-memory store for unit tests.
    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Run `f` on the blocking pool with exclusive use of the connection.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| anyhow!("bar store lock poisoned"))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| anyhow!("bar store task failed: {e}"))?
    }

    /// Number of stored bars (all timeframes).
    pub async fn count(&self) -> Result<i64> {
        self.run(|c| Ok(c.query_row("SELECT count(*) FROM bars", [], |r| r.get::<_, i64>(0))?)).await
    }
}
