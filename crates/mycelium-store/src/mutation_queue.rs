//! The MCP mutation queue: SQLite metadata rows for write-later items.
//!
//! Row ops only — payload blobs live in the FileRepo (raw payloads,
//! user scope, at /mutation-queue/<id>) and are managed by the queue
//! worker, never this module. Mirrors the ingest_jobs row pattern in
//! books.rs: conditional UPDATEs for atomicity, RFC 3339 timestamps.

use chrono::Utc;
use uuid::Uuid;

use crate::Store;
use crate::models::{MutationQueueItem, QueueStatus, QueueTool};

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("queue is at capacity")]
    Capacity,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, QueueError>;

/// Raw `mutation_queue` row shape for `query_as` (one field per column,
/// in SELECT order).
type QueueRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

impl Store {
    /// Enqueue: INSERT row in `staging` (the payload file does not
    /// exist yet). The caller must write the payload, then call
    /// `activate` to flip the row to pending.
    pub async fn enqueue(&self, user_id: Uuid, tool: QueueTool) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO mutation_queue (id, user_id, tool, status, attempts, detail, created_at, updated_at, next_retry_at, final_paths)
             VALUES (?, ?, ?, 'staging', 0, '', ?, ?, NULL, NULL)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(tool.as_str())
        .bind(&now)
        .bind(&now)
        .execute(self.pool())
        .await?;
        Ok(id)
    }

    /// Depth cap check + enqueue. The cap counts staging+pending+running
    /// rows; terminal rows don't count. SQLite's pool serialization makes
    /// the check-then-insert close enough under contention: a 1-row
    /// overshoot at extreme concurrency is accepted — the cap is a
    /// runaway guard, not an accounting invariant.
    pub async fn enqueue_capped(&self, user_id: Uuid, tool: QueueTool, cap: u32) -> Result<Uuid> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mutation_queue WHERE user_id = ? AND status IN ('staging','pending','running')",
        )
        .bind(user_id.to_string())
        .fetch_one(self.pool())
        .await?;
        if count >= i64::from(cap) {
            return Err(QueueError::Capacity);
        }
        self.enqueue(user_id, tool).await
    }

    /// Flip staging → pending (idempotent: allowed when the row is
    /// already pending — boot-sweep replays land here).
    pub async fn activate(&self, id: Uuid) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE mutation_queue SET status = 'pending', updated_at = ?
             WHERE id = ? AND status IN ('staging', 'pending')",
        )
        .bind(&now)
        .bind(id.to_string())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Claim this user's oldest due pending item, atomically: the
    /// conditional UPDATE is the atomicity (only one claimer wins).
    /// Attempts increment inside the claim so a lost race never
    /// double-counts.
    pub async fn claim_due_for_user(&self, user_id: Uuid) -> Result<Option<MutationQueueItem>> {
        let now = Utc::now().to_rfc3339();
        // Oldest due candidates; next_retry_at compares
        // lexicographically (RFC 3339 UTC; equal-format strings sort
        // correctly). Loop past rows another claimer took.
        let candidates: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM mutation_queue
             WHERE user_id = ? AND status = 'pending'
               AND (next_retry_at IS NULL OR next_retry_at <= ?)
             ORDER BY created_at LIMIT 8",
        )
        .bind(user_id.to_string())
        .bind(&now)
        .fetch_all(self.pool())
        .await?;
        for (id,) in candidates {
            let res = sqlx::query(
                "UPDATE mutation_queue
                 SET status = 'running', attempts = attempts + 1, updated_at = ?
                 WHERE id = ? AND status = 'pending'",
            )
            .bind(&now)
            .bind(&id)
            .execute(self.pool())
            .await?;
            if res.rows_affected() > 0 {
                let item_id = Uuid::parse_str(&id)
                    .map_err(|_| sqlx::Error::Decode("bad queue row id".into()))?;
                let item = self
                    .queue_item(user_id, item_id)
                    .await?
                    .expect("just claimed");
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    /// Terminal success: status done, final_paths JSON, provenance detail.
    pub async fn mark_done(&self, id: Uuid, paths: &[String], detail: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let paths_json = serde_json::to_string(paths).unwrap_or_else(|_| "[]".into());
        sqlx::query(
            "UPDATE mutation_queue SET status = 'done', final_paths = ?, detail = ?, updated_at = ?, next_retry_at = NULL
             WHERE id = ?",
        )
        .bind(&paths_json)
        .bind(detail)
        .bind(&now)
        .bind(id.to_string())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Transient failure: back to pending with a retry timer (attempts
    /// already incremented by the claim).
    pub async fn mark_failed(&self, id: Uuid, next_retry_at: String, detail: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE mutation_queue SET status = 'pending', next_retry_at = ?, detail = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&next_retry_at)
        .bind(detail)
        .bind(&now)
        .bind(id.to_string())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Terminal failure: dead (never auto-deleted; receipt + /health
    /// surface it).
    pub async fn mark_dead(&self, id: Uuid, detail: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE mutation_queue SET status = 'dead', detail = ?, updated_at = ?, next_retry_at = NULL
             WHERE id = ?",
        )
        .bind(detail)
        .bind(&now)
        .bind(id.to_string())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Receipt-scoped row lookup. USER-SCOPED on purpose (spec §4.1):
    /// `WHERE id = ? AND user_id = ?` so another user's id finds nothing
    /// (`None` — the receipt tool renders `state=unknown`, never leaking
    /// existence). The drain's internal uses always pass the row's own
    /// user, which by construction matches.
    pub async fn queue_item(&self, user_id: Uuid, id: Uuid) -> Result<Option<MutationQueueItem>> {
        let row: Option<QueueRow> = sqlx::query_as(
            "SELECT id, user_id, tool, status, attempts, detail, created_at, updated_at, next_retry_at, final_paths
             FROM mutation_queue WHERE id = ? AND user_id = ?",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .fetch_optional(self.pool())
        .await?;
        let Some((
            id,
            user_id,
            tool,
            status,
            attempts,
            detail,
            created_at,
            updated_at,
            next_retry_at,
            final_paths,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(MutationQueueItem {
            id: Uuid::parse_str(&id).map_err(|_| sqlx::Error::Decode("bad queue row id".into()))?,
            user_id: Uuid::parse_str(&user_id)
                .map_err(|_| sqlx::Error::Decode("bad queue row user_id".into()))?,
            tool: QueueTool::from_str(&tool)
                .ok_or_else(|| sqlx::Error::Decode(format!("bad tool {tool}").into()))?,
            status: QueueStatus::from_str(&status)
                .ok_or_else(|| sqlx::Error::Decode(format!("bad status {status}").into()))?,
            attempts: u32::try_from(attempts).unwrap_or(0),
            detail,
            created_at,
            updated_at,
            next_retry_at,
            final_paths,
        }))
    }

    /// Users with a due pending item, oldest item first.
    pub async fn queue_users_with_due(&self) -> Result<Vec<Uuid>> {
        let now = Utc::now().to_rfc3339();
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT user_id, MIN(created_at) AS oldest FROM mutation_queue
             WHERE status = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?)
             GROUP BY user_id ORDER BY oldest",
        )
        .bind(&now)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(u,)| Uuid::parse_str(&u).ok())
            .collect())
    }

    /// Staging rows (boot sweep: complete or fail them).
    pub async fn staging_rows(&self) -> Result<Vec<MutationQueueItem>> {
        self.rows_with_status("staging").await
    }

    /// Running rows (boot sweep: a dead process left them mid-flight).
    pub async fn running_rows(&self) -> Result<Vec<MutationQueueItem>> {
        self.rows_with_status("running").await
    }

    async fn rows_with_status(&self, status: &str) -> Result<Vec<MutationQueueItem>> {
        // Reuses queue_item's row mapping — select (id, user_id) then
        // fetch each row through the user-scoped lookup.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, user_id FROM mutation_queue WHERE status = ? ORDER BY created_at",
        )
        .bind(status)
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, user_id) in rows {
            let id =
                Uuid::parse_str(&id).map_err(|_| sqlx::Error::Decode("bad queue row id".into()))?;
            let user_id = Uuid::parse_str(&user_id)
                .map_err(|_| sqlx::Error::Decode("bad queue row user_id".into()))?;
            if let Some(item) = self.queue_item(user_id, id).await? {
                out.push(item);
            }
        }
        Ok(out)
    }

    /// Queue health snapshot for /health and /metrics: depth
    /// (staging+pending+running), oldest pending age in seconds,
    /// dead count, per-tool counts among non-terminal rows.
    pub async fn queue_health_counts(&self) -> Result<QueueHealth> {
        let depth: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mutation_queue WHERE status IN ('staging','pending','running')",
        )
        .fetch_one(self.pool())
        .await?;
        let oldest: Option<String> = sqlx::query_scalar(
            "SELECT MIN(created_at) FROM mutation_queue WHERE status = 'pending'",
        )
        .fetch_one(self.pool())
        .await?;
        let dead: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM mutation_queue WHERE status = 'dead'")
                .fetch_one(self.pool())
                .await?;
        let mut per_tool = Vec::new();
        for tool in [QueueTool::Add, QueueTool::Update, QueueTool::Maintain] {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM mutation_queue WHERE tool = ? AND status IN ('staging','pending','running')",
            )
            .bind(tool.as_str())
            .fetch_one(self.pool())
            .await?;
            per_tool.push((tool, n));
        }
        let oldest_age = oldest.map(|ts| {
            // Age from the RFC 3339 string; parse failures degrade to 0.
            chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|t| Utc::now().signed_duration_since(t).num_seconds().max(0))
                .unwrap_or(0)
        });
        Ok(QueueHealth {
            depth: usize::try_from(depth).unwrap_or(0),
            oldest_pending_age_seconds: oldest_age,
            dead_count: usize::try_from(dead).unwrap_or(0),
            per_tool,
        })
    }
}

/// Queue health snapshot (store-side; the web layer formats it).
#[derive(Debug, Clone, Default)]
pub struct QueueHealth {
    /// Non-terminal rows (staging + pending + running).
    pub depth: usize,
    /// Age of the oldest pending row, in seconds.
    pub oldest_pending_age_seconds: Option<i64>,
    /// Terminal dead rows (surfaced, never auto-deleted).
    pub dead_count: usize,
    /// (tool, non-terminal row count) for each tool.
    pub per_tool: Vec<(QueueTool, i64)>,
}
