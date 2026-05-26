//! SQLite-based request logging.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use chrono::Local;

/// A request record to be persisted.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub correlation_id: String,
    pub method: String,
    pub path: String,
    pub model: String,
    pub provider: String,
    pub duration_ms: f64,
    pub response_status: u16,
    pub streaming: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub api_key_hash: Option<String>,
}

impl RequestRecord {
    /// Create a new request record with common fields populated.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: String,
        model: String,
        provider: String,
        duration: std::time::Duration,
        response_status: u16,
        streaming: bool,
        token_stats: &crate::proxy::TokenStats,
        api_key_hash: Option<String>,
    ) -> Self {
        Self {
            correlation_id: uuid::Uuid::new_v4().to_string(),
            method: "POST".to_string(),
            path,
            model,
            provider,
            duration_ms: duration.as_secs_f64() * 1000.0,
            response_status,
            streaming,
            input_tokens: token_stats.input_tokens,
            output_tokens: token_stats.output_tokens,
            cache_read_tokens: token_stats.cache_read,
            cache_write_tokens: token_stats.cache_write,
            api_key_hash,
        }
    }
}

/// A usage row returned from aggregation queries.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub api_key_hash: String,
    pub model: String,
    pub period: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub request_count: u64,
}

/// Time grouping for usage queries.
#[derive(Debug, Clone, Copy)]
pub enum GroupBy {
    Day,
    Week,
    Month,
}

impl GroupBy {
    fn date_expr(&self) -> &'static str {
        match self {
            Self::Day => "date(created_at, 'localtime')",
            Self::Week => "strftime('%Y-W%W', created_at, 'localtime')",
            Self::Month => "strftime('%Y-%m', created_at, 'localtime')",
        }
    }
}

/// SQLite database for persisting request logs.
#[derive(Debug, Clone)]
pub struct Database {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl Database {
    /// Open (or create) the database at the given path and run migrations.
    pub async fn open(path: PathBuf) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create database directory: {}", parent.display())
            })?;
        }

        let path_clone = path.clone();
        let conn = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&path_clone)
                .with_context(|| format!("Failed to open database: {}", path_clone.display()))?;

            // Configure pragmas for performance
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA cache_size=10000;
                 PRAGMA busy_timeout=5000;",
            )
            .context("Failed to set database pragmas")?;

            Self::migrate(&conn)?;

            Ok::<_, anyhow::Error>(conn)
        })
        .await
        .context("Database init task panicked")??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                correlation_id TEXT NOT NULL,
                method TEXT NOT NULL,
                path TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                provider TEXT NOT NULL DEFAULT '',
                duration_ms REAL NOT NULL DEFAULT 0,
                response_status INTEGER NOT NULL DEFAULT 0,
                streaming INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                api_key_hash TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_requests_correlation_id ON requests(correlation_id);
            CREATE INDEX IF NOT EXISTS idx_requests_api_key ON requests(api_key_hash);
            CREATE INDEX IF NOT EXISTS idx_requests_created_at ON requests(created_at);",
        )
        .context("Failed to run database migrations")?;
        Ok(())
    }

    /// Insert a request record. Runs on the blocking thread pool.
    pub async fn insert_request(&self, record: RequestRecord) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO requests (correlation_id, method, path, model, provider,
                    duration_ms, response_status, streaming, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, api_key_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    record.correlation_id,
                    record.method,
                    record.path,
                    record.model,
                    record.provider,
                    record.duration_ms,
                    record.response_status,
                    record.streaming as i32,
                    record.input_tokens.map(|t| t as i64),
                    record.output_tokens.map(|t| t as i64),
                    record.cache_read_tokens.map(|t| t as i64),
                    record.cache_write_tokens.map(|t| t as i64),
                    record.api_key_hash,
                ],
            )
            .context("Failed to insert request record")?;
            drop(conn);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Request insert task panicked")??;
        Ok(())
    }

    /// Open the database in read-only mode (for CLI queries).
    pub fn open_readonly(path: &str) -> Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("Failed to open database: {path}"))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Query usage grouped by api_key_hash, model, and time period.
    pub async fn query_usage(
        &self,
        api_key_hash: Option<&str>,
        since: &str,
        group_by: GroupBy,
    ) -> Result<Vec<UsageRow>> {
        let conn = self.conn.clone();
        let api_key_hash = api_key_hash.map(String::from);
        let since = since.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let date_expr = group_by.date_expr();
            let key_clause = if api_key_hash.is_some() {
                "AND api_key_hash = ?2"
            } else {
                ""
            };

            let sql = format!(
                "SELECT COALESCE(api_key_hash, '') as key_hash, model, {date_expr} as period,
                    COALESCE(SUM(input_tokens), 0) as input_tokens,
                    COALESCE(SUM(output_tokens), 0) as output_tokens,
                    COALESCE(SUM(cache_read_tokens), 0) as cache_read_tokens,
                    COALESCE(SUM(cache_write_tokens), 0) as cache_write_tokens,
                    COUNT(*) as request_count
                 FROM requests
                 WHERE created_at >= datetime(?1, 'utc') {key_clause}
                 GROUP BY key_hash, model, period
                 ORDER BY period DESC, key_hash, model"
            );

            let params: Vec<Box<dyn rusqlite::types::ToSql>> =
                if let Some(ref key_hash) = api_key_hash {
                    vec![Box::new(since.clone()), Box::new(key_hash.clone())]
                } else {
                    vec![Box::new(since.clone())]
                };

            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn
                .prepare(&sql)
                .context("Failed to prepare usage query")?;
            let rows = stmt
                .query_map(params_refs.as_slice(), |row| {
                    Ok(UsageRow {
                        api_key_hash: row.get(0)?,
                        model: row.get(1)?,
                        period: row.get(2)?,
                        input_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                        output_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                        cache_read_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                        cache_write_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                        request_count: row.get::<_, i64>(7)?.max(0) as u64,
                    })
                })
                .context("Failed to query usage")?;

            let mut results = Vec::new();
            for row in rows {
                results.push(row.context("Failed to read usage row")?);
            }
            Ok::<_, anyhow::Error>(results)
        })
        .await
        .context("Usage query task panicked")?
    }

    /// SQL expression for summing all token columns.
    const TOTAL_TOKENS_EXPR: &'static str = "COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)+COALESCE(cache_read_tokens,0)+COALESCE(cache_write_tokens,0)), 0)";

    /// Load quota baselines by aggregating the requests table.
    /// Returns Vec<(api_key_hash, daily_tokens, monthly_tokens)> for all keys with activity.
    pub async fn load_quota_baselines(&self) -> Result<Vec<(String, u64, u64)>> {
        let conn = self.conn.clone();

        let now = Local::now();
        let local_day_start = format!("{} 00:00:00", now.date_naive());
        let local_month_start = format!(
            "{} 00:00:00",
            crate::quota::start_of_month(now.date_naive())
        );

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let daily_rows = Self::query_total_tokens_since(&conn, &local_day_start, None)?;
            let monthly_rows = Self::query_total_tokens_since(&conn, &local_month_start, None)?;

            drop(conn);

            // Merge daily and monthly into a single result set
            let mut map: std::collections::HashMap<String, (u64, u64)> =
                std::collections::HashMap::new();
            for (key, daily) in daily_rows {
                map.entry(key).or_insert((0, 0)).0 = daily;
            }
            for (key, monthly) in monthly_rows {
                map.entry(key).or_insert((0, 0)).1 = monthly;
            }

            let results: Vec<(String, u64, u64)> = map
                .into_iter()
                .map(|(key, (daily, monthly))| (key, daily, monthly))
                .collect();

            Ok::<_, anyhow::Error>(results)
        })
        .await
        .context("Quota baselines load task panicked")?
    }

    /// Load quota baseline for a single API key hash.
    /// Returns (daily_tokens, monthly_tokens).
    pub async fn load_quota_baseline_for_key(&self, key_hash: &str) -> Result<(u64, u64)> {
        let conn = self.conn.clone();
        let key_hash = key_hash.to_string();

        let now = Local::now();
        let local_day_start = format!("{} 00:00:00", now.date_naive());
        let local_month_start = format!(
            "{} 00:00:00",
            crate::quota::start_of_month(now.date_naive())
        );

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let daily_rows =
                Self::query_total_tokens_since(&conn, &local_day_start, Some(&key_hash))?;
            let monthly_rows =
                Self::query_total_tokens_since(&conn, &local_month_start, Some(&key_hash))?;

            let daily = daily_rows.first().map(|(_, v)| *v).unwrap_or(0);
            let monthly = monthly_rows.first().map(|(_, v)| *v).unwrap_or(0);

            drop(conn);

            Ok::<_, anyhow::Error>((daily, monthly))
        })
        .await
        .context("Single-key baseline query panicked")?
    }

    /// Shared helper: query total tokens per api_key_hash since a given timestamp.
    /// If `key_hash` is Some, filters to that specific key; otherwise returns all keys.
    fn query_total_tokens_since(
        conn: &rusqlite::Connection,
        since: &str,
        key_hash: Option<&str>,
    ) -> Result<Vec<(String, u64)>> {
        let key_clause = if key_hash.is_some() {
            "AND api_key_hash = ?2"
        } else {
            "AND api_key_hash IS NOT NULL"
        };
        let sql = format!(
            "SELECT api_key_hash, {}
             FROM requests
             WHERE created_at >= datetime(?1, 'utc') {key_clause}
             GROUP BY api_key_hash",
            Self::TOTAL_TOKENS_EXPR,
        );

        let mut stmt = conn
            .prepare(&sql)
            .context("Failed to prepare baseline query")?;
        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<(String, u64)> {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?.max(0) as u64,
            ))
        };
        let rows: Vec<(String, u64)> = if let Some(kh) = key_hash {
            stmt.query_map(rusqlite::params![since, kh], row_mapper)
        } else {
            stmt.query_map(rusqlite::params![since], row_mapper)
        }
        .context("Failed to query baselines")?
        .filter_map(|r| r.ok())
        .collect();

        Ok(rows)
    }

    /// Delete request logs older than the specified number of days.
    /// Returns the number of rows deleted.
    pub async fn cleanup_old_requests(&self, retention_days: u32) -> Result<u64> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let deleted: usize = conn
                .execute(
                    "DELETE FROM requests WHERE created_at < datetime('now', ?1)",
                    rusqlite::params![format!("-{retention_days} days")],
                )
                .context("Failed to delete old requests")?;
            drop(conn);
            Ok::<_, anyhow::Error>(deleted as u64)
        })
        .await
        .context("Cleanup task panicked")?
    }

    /// Check database connectivity.
    pub async fn health_check(&self) -> Result<String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let version: String = conn
                .query_row("SELECT sqlite_version()", [], |row| row.get(0))
                .context("Failed to query SQLite version")?;
            drop(conn);
            Ok::<_, anyhow::Error>(format!("SQLite {version}"))
        })
        .await
        .context("Health check task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_database_open_and_migrate() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(db_path).await.unwrap();
        let health = db.health_check().await.unwrap();
        assert!(health.starts_with("SQLite"));
    }

    #[tokio::test]
    async fn test_multiple_inserts() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(db_path).await.unwrap();

        for i in 0..5 {
            let record = RequestRecord {
                correlation_id: format!("req-{i}"),
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                model: "gpt-4o".to_string(),
                provider: "default".to_string(),
                duration_ms: 100.0 + i as f64,
                response_status: 200,
                streaming: false,
                input_tokens: Some(50),
                output_tokens: Some(25),
                cache_read_tokens: None,
                cache_write_tokens: None,
                api_key_hash: Some("abc123def456".to_string()),
            };
            db.insert_request(record).await.unwrap();
        }
    }
}
