use crate::models::{MonitorSnapshot, SnapshotHistoryEntry};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    generated_at INTEGER NOT NULL,
    lookback_days INTEGER NOT NULL,
    healthy_platforms INTEGER NOT NULL,
    configured_platforms INTEGER NOT NULL,
    total_audiences INTEGER NOT NULL,
    ready_audiences INTEGER NOT NULL,
    total_operations INTEGER NOT NULL,
    succeeded_operations INTEGER NOT NULL,
    total_campaigns INTEGER NOT NULL,
    active_campaigns INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_snapshots_generated_at ON snapshots(generated_at DESC);
"#;

pub struct SqliteCache {
    path_label: String,
    conn: Mutex<Connection>,
}

impl SqliteCache {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
            }
        }
        let conn =
            Connection::open(path).map_err(|e| format!("open sqlite {}: {e}", path.display()))?;
        Self::init(conn, path.display().to_string())
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn =
            Connection::open_in_memory().map_err(|e| format!("open in-memory sqlite: {e}"))?;
        Self::init(conn, ":memory:".to_string())
    }

    pub fn open_or_memory(db_path: Option<&str>) -> Self {
        match db_path {
            Some(raw) => {
                let p = PathBuf::from(raw);
                match Self::open(&p) {
                    Ok(cache) => cache,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to open SQLite cache at {raw} ({e}); falling back to in-memory SQLite"
                        );
                        Self::open_in_memory().expect("in-memory sqlite must initialize")
                    }
                }
            }
            None => Self::open_in_memory().expect("in-memory sqlite must initialize"),
        }
    }

    fn init(conn: Connection, path_label: String) -> Result<Self, String> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("set journal_mode=WAL: {e}"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("set synchronous=NORMAL: {e}"))?;
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("init sqlite schema: {e}"))?;
        Ok(Self {
            path_label,
            conn: Mutex::new(conn),
        })
    }

    pub fn path_label(&self) -> &str {
        &self.path_label
    }

    pub fn latest_snapshot(&self) -> Result<Option<MonitorSnapshot>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("sqlite mutex poisoned: {e}"))?;
        let json_str: Option<String> = conn
            .query_row(
                "SELECT payload_json FROM snapshots ORDER BY generated_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("query latest snapshot: {e}"))?;

        match json_str {
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| format!("deserialize cached snapshot: {e}")),
            None => Ok(None),
        }
    }

    pub fn save_snapshot(
        &self,
        snapshot: &MonitorSnapshot,
        retention_days: u32,
    ) -> Result<u64, String> {
        // Persist snapshot without volatile runtime cache metadata
        let mut to_store = snapshot.clone();
        to_store.cache = None;
        let payload_json =
            serde_json::to_string(&to_store).map_err(|e| format!("serialize snapshot: {e}"))?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("sqlite mutex poisoned: {e}"))?;

        conn.execute(
            r#"
            INSERT INTO snapshots (
                generated_at,
                lookback_days,
                healthy_platforms,
                configured_platforms,
                total_audiences,
                ready_audiences,
                total_operations,
                succeeded_operations,
                total_campaigns,
                active_campaigns,
                payload_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                to_store.generated_at,
                to_store.lookback_days as i64,
                to_store.summary.healthy_platforms as i64,
                to_store.summary.configured_platforms as i64,
                to_store.summary.total_audiences as i64,
                to_store.summary.ready_audiences as i64,
                to_store.summary.total_operations as i64,
                to_store.summary.succeeded_operations as i64,
                to_store.summary.total_campaigns as i64,
                to_store.summary.active_campaigns as i64,
                payload_json,
            ],
        )
        .map_err(|e| format!("insert snapshot: {e}"))?;

        let last_id = conn.last_insert_rowid();
        if retention_days == 0 {
            conn.execute("DELETE FROM snapshots WHERE id != ?1", params![last_id])
                .map_err(|e| format!("prune snapshots to latest: {e}"))?;
        } else {
            let cutoff = Utc::now().timestamp() - i64::from(retention_days) * 86_400;
            conn.execute(
                "DELETE FROM snapshots WHERE generated_at < ?1 AND id != ?2",
                params![cutoff, last_id],
            )
            .map_err(|e| format!("prune old snapshots: {e}"))?;
        }

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .map_err(|e| format!("count snapshots: {e}"))?;
        Ok(count.max(0) as u64)
    }

    pub fn snapshot_count(&self) -> Result<u64, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("sqlite mutex poisoned: {e}"))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .map_err(|e| format!("count snapshots: {e}"))?;
        Ok(count.max(0) as u64)
    }

    pub fn list_history(&self, limit: usize) -> Result<Vec<SnapshotHistoryEntry>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("sqlite mutex poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    id,
                    generated_at,
                    lookback_days,
                    healthy_platforms,
                    configured_platforms,
                    total_audiences,
                    ready_audiences,
                    total_operations,
                    succeeded_operations,
                    total_campaigns,
                    active_campaigns
                FROM snapshots
                ORDER BY generated_at DESC, id DESC
                LIMIT ?1
                "#,
            )
            .map_err(|e| format!("prepare history query: {e}"))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SnapshotHistoryEntry {
                    id: row.get(0)?,
                    generated_at: row.get(1)?,
                    lookback_days: row.get::<_, i64>(2)?.max(0) as u32,
                    healthy_platforms: row.get::<_, i64>(3)?.max(0) as usize,
                    configured_platforms: row.get::<_, i64>(4)?.max(0) as usize,
                    total_audiences: row.get::<_, i64>(5)?.max(0) as usize,
                    ready_audiences: row.get::<_, i64>(6)?.max(0) as usize,
                    total_operations: row.get::<_, i64>(7)?.max(0) as usize,
                    succeeded_operations: row.get::<_, i64>(8)?.max(0) as usize,
                    total_campaigns: row.get::<_, i64>(9)?.max(0) as usize,
                    active_campaigns: row.get::<_, i64>(10)?.max(0) as usize,
                })
            })
            .map_err(|e| format!("query history: {e}"))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read history row: {e}"))?);
        }
        Ok(out)
    }
}
