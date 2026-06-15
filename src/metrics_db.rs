use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::data_dir;

struct Inner {
    conn: Mutex<Connection>,
    // (instance_id, player_name) -> session row id
    active_sessions: Mutex<HashMap<(String, String), i64>>,
}

#[derive(Clone)]
pub struct MetricsDb(Arc<Inner>);

pub struct MetricRow {
    pub timestamp: i64,
    pub ram_mb: Option<u64>,
    pub tps: Option<f64>,
    pub player_count: i64,
    pub cpu_pct: Option<f64>,
}

pub struct EventRow {
    pub timestamp: i64,
    pub event_type: String,
}

pub struct PlayerStats {
    pub player_name: String,
    pub total_sessions: i64,
    pub total_secs: i64,
    pub last_seen: i64,
}

pub struct SessionRow {
    pub player_name: String,
    pub joined_at: i64,
    pub left_at: Option<i64>,
    pub duration_secs: Option<i64>,
}

const SCHEMA: &str = "
    PRAGMA journal_mode=WAL;
    CREATE TABLE IF NOT EXISTS metrics (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id  TEXT    NOT NULL,
        timestamp    INTEGER NOT NULL,
        ram_mb       INTEGER,
        tps          REAL,
        player_count INTEGER NOT NULL DEFAULT 0,
        cpu_pct      REAL
    );
    CREATE INDEX IF NOT EXISTS idx_metrics ON metrics(instance_id, timestamp);
    CREATE TABLE IF NOT EXISTS server_events (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id  TEXT    NOT NULL,
        timestamp    INTEGER NOT NULL,
        event_type   TEXT    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_events ON server_events(instance_id, timestamp);
    CREATE TABLE IF NOT EXISTS player_sessions (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id  TEXT    NOT NULL,
        player_name  TEXT    NOT NULL,
        joined_at    INTEGER NOT NULL,
        left_at      INTEGER,
        duration_secs INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_sessions ON player_sessions(instance_id, player_name);
";

const SCHEMA_MEMORY: &str = "
    CREATE TABLE IF NOT EXISTS metrics (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id TEXT NOT NULL, timestamp INTEGER NOT NULL,
        ram_mb INTEGER, tps REAL, player_count INTEGER NOT NULL DEFAULT 0, cpu_pct REAL
    );
    CREATE TABLE IF NOT EXISTS server_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id TEXT NOT NULL, timestamp INTEGER NOT NULL, event_type TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS player_sessions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        instance_id TEXT NOT NULL, player_name TEXT NOT NULL,
        joined_at INTEGER NOT NULL, left_at INTEGER, duration_secs INTEGER
    );
";

impl MetricsDb {
    pub fn open() -> Result<Self, rusqlite::Error> {
        let db_path = data_dir().join("metrics.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        // Migrate existing DBs that lack the cpu_pct column
        let _ = conn.execute_batch("ALTER TABLE metrics ADD COLUMN cpu_pct REAL;");
        Ok(MetricsDb(Arc::new(Inner {
            conn: Mutex::new(conn),
            active_sessions: Mutex::new(HashMap::new()),
        })))
    }

    pub fn open_memory() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory SQLite failed");
        let _ = conn.execute_batch(SCHEMA_MEMORY);
        MetricsDb(Arc::new(Inner {
            conn: Mutex::new(conn),
            active_sessions: Mutex::new(HashMap::new()),
        }))
    }

    pub fn record_metric(
        &self,
        instance_id: &str,
        timestamp: i64,
        ram_mb: Option<u64>,
        tps: Option<f32>,
        player_count: usize,
        cpu_pct: Option<f32>,
    ) {
        if let Ok(conn) = self.0.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO metrics (instance_id, timestamp, ram_mb, tps, player_count, cpu_pct)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    instance_id,
                    timestamp,
                    ram_mb.map(|v| v as i64),
                    tps.map(|v| v as f64),
                    player_count as i64,
                    cpu_pct.map(|v| v as f64),
                ],
            );
        }
    }

    pub fn record_event(&self, instance_id: &str, timestamp: i64, event_type: &str) {
        if let Ok(conn) = self.0.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO server_events (instance_id, timestamp, event_type) VALUES (?1, ?2, ?3)",
                params![instance_id, timestamp, event_type],
            );
        }
    }

    pub fn record_player_join(&self, instance_id: &str, player: &str, ts: i64) {
        let row_id = if let Ok(conn) = self.0.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO player_sessions (instance_id, player_name, joined_at) VALUES (?1, ?2, ?3)",
                params![instance_id, player, ts],
            );
            conn.last_insert_rowid()
        } else {
            return;
        };
        if let Ok(mut sessions) = self.0.active_sessions.lock() {
            sessions.insert((instance_id.to_string(), player.to_string()), row_id);
        }
    }

    pub fn record_player_leave(&self, instance_id: &str, player: &str, ts: i64) {
        let key = (instance_id.to_string(), player.to_string());
        let row_id = if let Ok(mut sessions) = self.0.active_sessions.lock() {
            sessions.remove(&key)
        } else {
            None
        };
        if let Some(id) = row_id {
            if let Ok(conn) = self.0.conn.lock() {
                let _ = conn.execute(
                    "UPDATE player_sessions SET left_at = ?1, duration_secs = ?1 - joined_at WHERE id = ?2",
                    params![ts, id],
                );
            }
        }
    }

    pub fn query_metrics(&self, instance_id: &str, since: i64) -> Vec<MetricRow> {
        let conn = match self.0.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT timestamp, ram_mb, tps, player_count, cpu_pct
             FROM metrics WHERE instance_id = ?1 AND timestamp >= ?2
             ORDER BY timestamp ASC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![instance_id, since], |row| {
            Ok(MetricRow {
                timestamp:    row.get(0)?,
                ram_mb:       row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                tps:          row.get(2)?,
                player_count: row.get(3)?,
                cpu_pct:      row.get(4)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn query_events(&self, instance_id: &str, since: i64) -> Vec<EventRow> {
        let conn = match self.0.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT timestamp, event_type FROM server_events
             WHERE instance_id = ?1 AND timestamp >= ?2
             ORDER BY timestamp ASC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![instance_id, since], |row| {
            Ok(EventRow {
                timestamp:  row.get(0)?,
                event_type: row.get(1)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn query_player_stats(&self, instance_id: &str, since: i64) -> Vec<PlayerStats> {
        let conn = match self.0.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT player_name,
                    COUNT(*) as total_sessions,
                    COALESCE(SUM(duration_secs), 0) as total_secs,
                    MAX(joined_at) as last_seen
             FROM player_sessions
             WHERE instance_id = ?1 AND joined_at >= ?2
             GROUP BY player_name
             ORDER BY total_secs DESC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![instance_id, since], |row| {
            Ok(PlayerStats {
                player_name:    row.get(0)?,
                total_sessions: row.get(1)?,
                total_secs:     row.get(2)?,
                last_seen:      row.get(3)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn query_recent_sessions(&self, instance_id: &str, limit: i64) -> Vec<SessionRow> {
        let conn = match self.0.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT player_name, joined_at, left_at, duration_secs
             FROM player_sessions WHERE instance_id = ?1
             ORDER BY joined_at DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![instance_id, limit], |row| {
            Ok(SessionRow {
                player_name:  row.get(0)?,
                joined_at:    row.get(1)?,
                left_at:      row.get(2)?,
                duration_secs: row.get(3)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn cleanup_old(&self, older_than: i64) {
        if let Ok(conn) = self.0.conn.lock() {
            let _ = conn.execute("DELETE FROM metrics WHERE timestamp < ?1", params![older_than]);
            let _ = conn.execute("DELETE FROM server_events WHERE timestamp < ?1", params![older_than]);
            let _ = conn.execute("DELETE FROM player_sessions WHERE joined_at < ?1", params![older_than]);
        }
    }
}
