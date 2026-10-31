//! The MCP mutation queue: SQLite metadata rows for write-later items.
//!
//! Row ops only — payload blobs live in the FileRepo (raw payloads,
//! user scope, at /mutation-queue/<id>) and are managed by the queue
//! worker, never this module. Mirrors the ingest_jobs row pattern in
//! books.rs: conditional UPDATEs for atomicity, RFC 3339 timestamps.

use chrono::Utc;
use mycelium_crypto::keys::MasterKey;
use uuid::Uuid;

use crate::Store;
use crate::file_repo::{FileRepo, FileRepoError, Scope};
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
    String,
);

/// Raw row shape for the two admin scans (`queue_waiting_rows`,
/// `queue_recent_rows`) — the seven joined columns in SELECT order.
type AdminRow = (String, String, String, String, i64, String, String);

/// The shared decode behind both admin scans: a bad id/tool/status is a
/// hard Decode error (a same-version server never writes one), and the
/// column's empty `detail` default reads as `None` — "no error yet",
/// which every consumer renders as nothing.
fn decode_admin_row(row: AdminRow) -> Result<QueueWaitingRow> {
    let (id, username, tool, status, attempts, detail, created_at) = row;
    Ok(QueueWaitingRow {
        id: Uuid::parse_str(&id).map_err(|_| sqlx::Error::Decode("bad queue row id".into()))?,
        username,
        tool: QueueTool::from_str(&tool)
            .ok_or_else(|| sqlx::Error::Decode(format!("bad tool {tool}").into()))?,
        status: QueueStatus::from_str(&status)
            .ok_or_else(|| sqlx::Error::Decode(format!("bad status {status}").into()))?,
        attempts: u32::try_from(attempts).unwrap_or(0),
        detail: if detail.is_empty() {
            None
        } else {
            Some(detail)
        },
        created_at,
    })
}

impl Store {
    /// Enqueue: INSERT row in `staging` (the payload file does not
    /// exist yet). The caller must write the payload, then call
    /// `activate` to flip the row to pending. `source` is the
    /// provenance label captured with the row (`mcp:<key-label>`,
    /// `web:<username>`, …) — stamped into concept frontmatter at
    /// integrate time (spec §3.1); an empty string is unknown
    /// provenance.
    pub async fn enqueue(&self, user_id: Uuid, tool: QueueTool, source: &str) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO mutation_queue (id, user_id, tool, source, status, attempts, detail, created_at, updated_at, next_retry_at, final_paths)
             VALUES (?, ?, ?, ?, 'staging', 0, '', ?, ?, NULL, NULL)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(tool.as_str())
        .bind(source)
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
    pub async fn enqueue_capped(
        &self,
        user_id: Uuid,
        tool: QueueTool,
        cap: u32,
        source: &str,
    ) -> Result<Uuid> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mutation_queue WHERE user_id = ? AND status IN ('staging','pending','running')",
        )
        .bind(user_id.to_string())
        .fetch_one(self.pool())
        .await?;
        if count >= i64::from(cap) {
            return Err(QueueError::Capacity);
        }
        self.enqueue(user_id, tool, source).await
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
            "SELECT id, user_id, tool, status, attempts, detail, created_at, updated_at, next_retry_at, final_paths, source
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
            source,
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
            source,
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

    /// Admin scan (outage-recovery spec §3.5): every queue item still
    /// waiting to be integrated — `staging` + `pending` (never
    /// `running`: a claimed item is being worked, not waiting) — with
    /// the owner's username joined from `users`, oldest first. Metadata
    /// only: no payload, no args JSON; `detail` is the bounded
    /// `short_detail` failure/provenance text the drain persists.
    pub async fn queue_waiting_rows(&self) -> Result<Vec<QueueWaitingRow>> {
        let rows: Vec<AdminRow> = sqlx::query_as(
            "SELECT q.id, u.username, q.tool, q.status, q.attempts, q.detail, q.created_at
             FROM mutation_queue q
             JOIN users u ON u.id = q.user_id
             WHERE q.status IN ('staging', 'pending')
             ORDER BY q.created_at",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(decode_admin_row).collect()
    }

    /// Receipts panel (`/status`): the most recent queue items of EVERY
    /// status — `staging`/`pending`/`running` plus the terminal
    /// `done`/`dead` the waiting scan hides — newest enqueue first,
    /// capped at `limit`. Same join, columns and decode guards as
    /// `queue_waiting_rows` (which stays the System page's waiting-only
    /// source); ordered by `created_at` because that is the enqueue time
    /// the panel shows an age from, and the only timestamp
    /// `QueueWaitingRow` carries. Metadata only, exactly like the
    /// waiting scan: no payload, no args JSON.
    pub async fn queue_recent_rows(&self, limit: u32) -> Result<Vec<QueueWaitingRow>> {
        let rows: Vec<AdminRow> = sqlx::query_as(
            "SELECT q.id, u.username, q.tool, q.status, q.attempts, q.detail, q.created_at
             FROM mutation_queue q
             JOIN users u ON u.id = q.user_id
             ORDER BY q.created_at DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(decode_admin_row).collect()
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

    /// Enqueue/integration counters over ALL rows (every status counts —
    /// a Prometheus counter must never go down, so done/dead rows stay
    /// counted). The /metrics scrape source: SQL COUNTs, deliberately
    /// not the worker's in-memory atomics — atomics reset on restart
    /// while rows persist.
    pub async fn queue_totals(&self) -> Result<QueueTotals> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT tool, COUNT(*) FROM mutation_queue GROUP BY tool")
                .fetch_all(self.pool())
                .await?;
        let mut queued_per_tool: Vec<(QueueTool, u64)> = vec![
            (QueueTool::Add, 0),
            (QueueTool::Update, 0),
            (QueueTool::Maintain, 0),
        ];
        for (tool, count) in rows {
            if let Some(t) = QueueTool::from_str(&tool)
                && let Some(pos) = queued_per_tool.iter().position(|(s, _)| *s == t)
            {
                queued_per_tool[pos].1 += count as u64;
            }
        }
        let (integrated,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mutation_queue WHERE status = 'done' \
             AND detail NOT LIKE 'integrated via deterministic fallback%'",
        )
        .fetch_one(self.pool())
        .await?;
        let (fallback,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mutation_queue WHERE status = 'done' \
             AND detail LIKE 'integrated via deterministic fallback%'",
        )
        .fetch_one(self.pool())
        .await?;
        let (dead,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mutation_queue WHERE status = 'dead'")
                .fetch_one(self.pool())
                .await?;
        Ok(QueueTotals {
            queued_per_tool,
            integrated: integrated as u64,
            fallback: fallback as u64,
            dead: dead as u64,
        })
    }
}

/// The queued tool's args + user content for a mutation-queue item.
/// Serialized as JSON into the item's FileRepo payload blob at
/// /mutation-queue/<id>. `args_json` holds the original tool args
/// verbatim (a JSON object string), so the drain rebuilds the exact
/// instruction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MutationPayload {
    pub tool: QueueTool,
    pub args_json: String,
    pub content: String,
}

/// The canonical payload path for a queue item (raw blob — NOT `.md`,
/// so it never collides with the staging note below).
pub fn payload_path(id: Uuid) -> String {
    format!("/mutation-queue/{id}")
}

/// The canonical concept path for an `add` item's staging note. The
/// caller writes it through `ConceptStore::put_batch_internal` with a
/// caller-derived title (the store layer is title-agnostic).
pub fn stage_note_path(id: Uuid) -> String {
    format!("/mutation-queue/{id}.md")
}

/// Write a queue payload as a raw FileRepo blob (user scope). NOT a
/// concept: no registry row, no index entry, no index.md regen.
pub async fn write_payload(
    repo: &FileRepo,
    master_key: &MasterKey,
    id: Uuid,
    content: &MutationPayload,
) -> std::result::Result<(), FileRepoError> {
    let bytes = serde_json::to_vec(content).map_err(|e| {
        FileRepoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })?;
    repo.write(&payload_path(id), &bytes, &Scope::User(master_key.clone()))
        .await
}

/// Read a queue payload. `None` when the blob is absent — a wrong
/// master key also derives a different opaque stored filename, so a
/// wrong key is indistinguishable from a missing blob (no existence
/// leak); a deserialize failure maps to `None` as well, and the drain
/// fails fast with "payload missing" for a row whose blob is
/// unparseable (a same-version server never writes one, but a
/// corrupted store may hit one).
pub async fn read_payload(
    repo: &FileRepo,
    master_key: &MasterKey,
    id: Uuid,
) -> std::result::Result<Option<MutationPayload>, FileRepoError> {
    match repo
        .read(&payload_path(id), &Scope::User(master_key.clone()))
        .await
    {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(FileRepoError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// One row of the admin waiting scan (`queue_waiting_rows` output).
/// Metadata only — no payload, no args JSON; `detail` is the bounded
/// drain failure text (`short_detail` convention).
#[derive(Debug, Clone)]
pub struct QueueWaitingRow {
    pub id: Uuid,
    pub username: String,
    pub tool: QueueTool,
    pub status: QueueStatus,
    pub attempts: u32,
    /// None when the row has never failed (the column's empty default).
    pub detail: Option<String>,
    /// RFC 3339 enqueue timestamp (the waited-seconds base the page
    /// derives; same raw-string convention as `AdminWaitingTurn`).
    pub created_at: String,
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

/// All-rows queue counters (store-side; the web layer formats them).
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueTotals {
    /// Rows ever enqueued, per tool (every status counts — a counter
    /// must never decrease, so terminal rows stay counted).
    pub queued_per_tool: Vec<(QueueTool, u64)>,
    /// Done rows integrated by an agent run.
    pub integrated: u64,
    /// Done rows integrated by the deterministic fallback.
    pub fallback: u64,
    /// Dead rows.
    pub dead: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enqueue_carries_source_through_item() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // A user row to satisfy the queue's FK (mirrors the
        // integration suite's seed_user).
        let user = uuid::Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
             VALUES (?, ?, ?, 'user', 'local', NULL, ?, 0, NULL, ?, ?)",
        )
        .bind(user.to_string())
        .bind("queue-source-test")
        .bind("queue-source-test@t")
        .bind("placeholder-sealed-master-key")
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();
        let id = store
            .enqueue_capped(user, QueueTool::Add, 10, "mcp:e2e-key")
            .await
            .unwrap();
        let item = store
            .queue_item(user, id)
            .await
            .unwrap()
            .expect("row round-trips");
        assert_eq!(item.source, "mcp:e2e-key");
    }
}
