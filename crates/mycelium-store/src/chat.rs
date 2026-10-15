//! Chat session history: SQLite metadata rows for per-user chat
//! transcripts.
//!
//! Row ops only — content is sealed elsewhere. Titles and message
//! bodies travel as hex AEAD envelopes under the purpose-bound chat DEK
//! (`ChatKeys::from_master_key`), so SQLite holds metadata only and the
//! database never sees a title or a message body. The AAD binds each
//! envelope to its own row: `chat:<session>:title` for titles and
//! `chat:<session>:<seq>` for messages — the seq is assigned inside the
//! writing transaction, then the envelope is sealed with it, so a blob
//! cannot be replayed into another row.
//!
//! Mirrors mutation_queue.rs: impl blocks on a pool-holding struct,
//! `?`-propagated sqlx errors, RFC 3339 timestamps, transactions via
//! `pool.begin()`. Every user-facing query filters `chat_sessions.user_id`
//! — a foreign session id is indistinguishable from an unknown one.
//!
//! Turn state machine (spec D3/D6/D8): exactly one `running` turn per
//! session (unique partial index — a second `create_turn` is a
//! `Conflict`); a boot sweep marks running turns `interrupted` (the
//! resume worker claims those once: `interrupted` with `attempts = 0`);
//! a new submit supersedes a resumable interrupted turn → `abandoned`.

use mycelium_crypto::CryptoError;
use mycelium_crypto::chat_keys::{ChatKeys, open_chat, seal_chat};
use mycelium_crypto::keys::MasterKey;
use uuid::Uuid;

/// Cap on sessions per user (spec §4): `create_session` auto-prunes the
/// oldest prunable session when the user is at the cap — never a
/// caller-visible rejection.
pub const MAX_SESSIONS_PER_USER: usize = 100;

/// A chat message's role (the `chat_messages.role` CHECK values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Tool,
    Assistant,
    Error,
}

impl ChatRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tool => "tool",
            Self::Assistant => "assistant",
            Self::Error => "error",
        }
    }

    // Deliberate inherent `from_str -> Option` (the `QueueStatus`
    // contract in models.rs), not `std::str::FromStr`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "tool" => Some(Self::Tool),
            "assistant" => Some(Self::Assistant),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A chat turn's status (the `chat_turns.status` CHECK values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatTurnStatus {
    Running,
    Done,
    Failed,
    Interrupted,
    Abandoned,
}

impl ChatTurnStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Abandoned => "abandoned",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }
}

/// A decrypted transcript row (seq order; `messages_after` output).
#[derive(Debug, Clone)]
pub struct ChatMessageRow {
    pub seq: i64,
    pub role: ChatRole,
    pub content: String,
}

/// A session row for the sidebar (`list_sessions` output).
#[derive(Debug, Clone)]
pub struct ChatSessionSummary {
    pub id: Uuid,
    pub title: String,
    pub updated_at: String,
}

/// A turn row (detached-turn state machine metadata).
#[derive(Debug, Clone)]
pub struct ChatTurnRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_seq: i64,
    pub status: ChatTurnStatus,
    pub attempts: i32,
}

/// Session metadata for the JS liveness poll (`session_meta` output).
#[derive(Debug, Clone)]
pub struct ChatSessionMeta {
    pub id: Uuid,
    pub title: String,
    /// The session's latest non-abandoned turn's status; None when the
    /// session has no interesting turn.
    pub turn_status: Option<ChatTurnStatus>,
    /// The latest turn is `interrupted` with `attempts = 0` — the resume
    /// worker may still claim it.
    pub turn_resumable: bool,
    /// `MAX(seq)` over the transcript (the `?after=` cursor base).
    pub max_seq: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    #[error("chat session not found")]
    NotFound,
    #[error("a turn is already running in this session")]
    Conflict,
}

type Result<T> = std::result::Result<T, ChatError>;

/// Title rendered when a sealed blob will not open under the provided
/// key (a wrong/rotated key must degrade the read, never fail it).
const UNREADABLE_TITLE: &str = "(unreadable)";

/// RFC 3339 UTC now — the house timestamp format.
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Raw `chat_turns` row shape for `query_as` (one field per column, in
/// SELECT order).
type TurnRowRaw = (String, String, i64, String, i32);

fn turn_row((id, session_id, user_seq, status, attempts): TurnRowRaw) -> Result<ChatTurnRow> {
    Ok(ChatTurnRow {
        id: Uuid::parse_str(&id).map_err(|_| sqlx::Error::Decode("bad chat turn id".into()))?,
        session_id: Uuid::parse_str(&session_id)
            .map_err(|_| sqlx::Error::Decode("bad chat turn session_id".into()))?,
        user_seq,
        status: ChatTurnStatus::from_str(&status)
            .ok_or_else(|| sqlx::Error::Decode(format!("bad turn status {status}").into()))?,
        attempts,
    })
}

/// The AAD for a message envelope (binding, per the sealing convention).
fn message_aad(session_id: Uuid, seq: i64) -> String {
    format!("chat:{session_id}:{seq}")
}

/// The AAD for a title envelope.
fn title_aad(session_id: Uuid) -> String {
    format!("chat:{session_id}:title")
}

/// Seal + hex-encode chat content (the config.rs envelope precedent —
/// TEXT columns, never raw bytes).
fn seal_hex(plain: &str, aad: &str, dek: &mycelium_crypto::keys::Dek) -> Result<String> {
    let sealed = seal_chat(plain.as_bytes(), aad, dek)?;
    Ok(hex::encode(sealed))
}

/// Hex-decode + open a chat envelope. The AAD and DEK are the caller's
/// row binding and key — a mismatch fails here (the AEAD tamper check).
fn open_hex(sealed: &str, aad: &str, dek: &mycelium_crypto::keys::Dek) -> Result<String> {
    let bytes = hex::decode(sealed).map_err(|_| CryptoError::Decrypt("not hex".into()))?;
    let plain = open_chat(&bytes, aad, dek)?;
    Ok(String::from_utf8(plain).map_err(|_| CryptoError::Decrypt("chat row not UTF-8".into()))?)
}

/// Chat session history over the chat_* tables (Task 1's migration).
///
/// Holds the pool only; keys are derived per call from the caller's
/// master key — no key material is stored here.
#[derive(Debug, Clone)]
pub struct ChatStore {
    pool: sqlx::SqlitePool,
}

impl ChatStore {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a session for the user with a derived title (the caller
    /// derives/truncates — the store stores what it is given).
    ///
    /// When the user is at the session cap, the oldest prunable session
    /// is deleted first: newest-by-`updated_at`, skipping sessions with
    /// a `running` turn or a resumable (`interrupted`, `attempts = 0`)
    /// turn — never delete a session whose turn still owes an answer.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        master: &MasterKey,
        title: &str,
    ) -> Result<Uuid> {
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        let mut tx = self.pool.begin().await?;
        // Prune: count, then delete prunable sessions oldest-first until
        // the count is under the cap. SQLite's single-writer +
        // serialized pool makes the check-then-delete safe; a
        // concurrent create overshooting by one is accepted (runaway
        // guard, not an accounting invariant).
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_sessions WHERE user_id = ?")
            .bind(user_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        let cap = i64::try_from(MAX_SESSIONS_PER_USER).unwrap_or(i64::MAX);
        if count >= cap {
            // Prunable: no running turn, no resumable interrupted turn.
            // The `updated_at` ASC ordering + NOT EXISTS guard picks
            // exactly the oldest debt-free sessions.
            sqlx::query(
                "DELETE FROM chat_sessions WHERE id IN (
                    SELECT s.id FROM chat_sessions s
                    WHERE s.user_id = ?
                      AND NOT EXISTS (
                          SELECT 1 FROM chat_turns t
                          WHERE t.session_id = s.id
                            AND (t.status = 'running'
                                 OR (t.status = 'interrupted' AND t.attempts = 0))
                      )
                    ORDER BY s.updated_at ASC
                    LIMIT ?
                )",
            )
            .bind(user_id.to_string())
            .bind(count + 1 - cap)
            .execute(&mut *tx)
            .await?;
        }
        let id = Uuid::new_v4();
        let ts = now();
        let sealed = seal_hex(title, &title_aad(id), &dek)?;
        sqlx::query(
            "INSERT INTO chat_sessions (id, user_id, title_sealed, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(&sealed)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The user's sessions, newest-first by `updated_at` (the sidebar).
    ///
    /// A title that fails to unseal renders as `(unreadable)` —
    /// skip-not-fail: a corrupted/rotated row degrades that one title,
    /// never the whole list.
    pub async fn list_sessions(
        &self,
        user_id: Uuid,
        master: &MasterKey,
    ) -> Result<Vec<ChatSessionSummary>> {
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, title_sealed, updated_at FROM chat_sessions
             WHERE user_id = ? ORDER BY updated_at DESC",
        )
        .bind(user_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, title_sealed, updated_at)| {
                let id = Uuid::parse_str(&id).ok()?;
                let title = open_hex(&title_sealed, &title_aad(id), &dek)
                    .unwrap_or_else(|_| UNREADABLE_TITLE.to_string());
                Some(ChatSessionSummary {
                    id,
                    title,
                    updated_at,
                })
            })
            .collect())
    }

    /// One session's metadata: title + liveness-poll turn fields. The
    /// turn fields come from the session's latest non-abandoned turn.
    pub async fn session_meta(
        &self,
        user_id: Uuid,
        master: &MasterKey,
        session_id: Uuid,
    ) -> Result<ChatSessionMeta> {
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        // Ownership check + title read in one ownership-scoped query.
        let row: Option<(String,)> =
            sqlx::query_as("SELECT title_sealed FROM chat_sessions WHERE id = ? AND user_id = ?")
                .bind(session_id.to_string())
                .bind(user_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let Some((title_sealed,)) = row else {
            return Err(ChatError::NotFound);
        };
        let title = open_hex(&title_sealed, &title_aad(session_id), &dek)
            .unwrap_or_else(|_| UNREADABLE_TITLE.to_string());
        // Latest non-abandoned turn (newest by created_at), then the
        // transcript's max seq.
        let turn = self.latest_turn(user_id, session_id).await?;
        let (turn_status, turn_resumable) = match turn {
            Some(t) if t.status != ChatTurnStatus::Abandoned => {
                let resumable = t.status == ChatTurnStatus::Interrupted && t.attempts == 0;
                (Some(t.status), resumable)
            }
            _ => (None, false),
        };
        let max_seq: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM chat_messages WHERE session_id = ?")
                .bind(session_id.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(ChatSessionMeta {
            id: session_id,
            title,
            turn_status,
            turn_resumable,
            max_seq: max_seq.unwrap_or(0),
        })
    }

    /// Delete a session the user owns. `Conflict` while a turn is
    /// running (delete-while-running would strand the runner's final
    /// writes); otherwise the FK cascade removes messages + turns.
    pub async fn delete_session(&self, user_id: Uuid, session_id: Uuid) -> Result<()> {
        // Running turn → refuse before deleting (scoped to the owner's
        // session, so a foreign id is just NotFound).
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chat_turns t
             JOIN chat_sessions s ON s.id = t.session_id
             WHERE t.session_id = ? AND s.user_id = ? AND t.status = 'running'",
        )
        .bind(session_id.to_string())
        .bind(user_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        if running > 0 {
            return Err(ChatError::Conflict);
        }
        // Ownership-scoped delete: 0 rows affected = unknown or foreign.
        let res = sqlx::query("DELETE FROM chat_sessions WHERE id = ? AND user_id = ?")
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(ChatError::NotFound);
        }
        Ok(())
    }

    /// Transcript rows with `seq > after_seq` in seq order (the replay /
    /// history read). Rows that fail to unseal are skipped — a corrupted
    /// row degrades one message, never the read.
    pub async fn messages_after(
        &self,
        user_id: Uuid,
        master: &MasterKey,
        session_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<ChatMessageRow>> {
        // Ownership gate: a foreign session id must be NotFound.
        let owned: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM chat_sessions WHERE id = ? AND user_id = ?")
                .bind(session_id.to_string())
                .bind(user_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if owned.is_none() {
            return Err(ChatError::NotFound);
        }
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT seq, role, content_sealed FROM chat_messages
             WHERE session_id = ? AND seq > ? ORDER BY seq",
        )
        .bind(session_id.to_string())
        .bind(after_seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(seq, role, content_sealed)| {
                let role = ChatRole::from_str(&role)?;
                let content =
                    open_hex(&content_sealed, &message_aad(session_id, seq), &dek).ok()?;
                Some(ChatMessageRow { seq, role, content })
            })
            .collect())
    }

    /// Submit a user message: ONE transaction that supersedes resumable
    /// interrupted turns (D8), inserts the user message (seq =
    /// `MAX(seq)+1`, bumping the session's `updated_at`), and inserts
    /// the `running` turn. The unique partial index on running turns is
    /// the double-submit guard → `Conflict`.
    pub async fn create_turn(
        &self,
        user_id: Uuid,
        master: &MasterKey,
        session_id: Uuid,
        message: &str,
    ) -> Result<ChatTurnRow> {
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        let mut tx = self.pool.begin().await?;
        // Ownership gate first, inside the tx (the write is scoped).
        let owned: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM chat_sessions WHERE id = ? AND user_id = ?")
                .bind(session_id.to_string())
                .bind(user_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if owned.is_none() {
            return Err(ChatError::NotFound);
        }
        // (a) Supersede any resumable interrupted turn of this session.
        sqlx::query("UPDATE chat_turns SET status = 'abandoned', updated_at = ? WHERE session_id = ? AND status = 'interrupted'")
            .bind(now())
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await?;
        // (b) Insert the user message: assign seq inside the tx, seal
        // the content with the assigned seq as AAD, bump updated_at.
        let max_seq: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM chat_messages WHERE session_id = ?")
                .bind(session_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let seq = max_seq.unwrap_or(0) + 1;
        let sealed = seal_hex(message, &message_aad(session_id, seq), &dek)?;
        let ts = now();
        sqlx::query(
            "INSERT INTO chat_messages (session_id, seq, role, content_sealed, created_at)
             VALUES (?, ?, 'user', ?, ?)",
        )
        .bind(session_id.to_string())
        .bind(seq)
        .bind(&sealed)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE chat_sessions SET updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(&ts)
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(&mut *tx)
            .await?;
        // (c) Insert the running turn. The unique partial index
        // (one running turn per session) rejects a concurrent submit.
        let turn_id = Uuid::new_v4();
        let res = sqlx::query(
            "INSERT INTO chat_turns (id, session_id, user_seq, status, attempts, created_at, updated_at)
             VALUES (?, ?, ?, 'running', 0, ?, ?)",
        )
        .bind(turn_id.to_string())
        .bind(session_id.to_string())
        .bind(seq)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            let unique = e
                .as_database_error()
                .map(|d| d.is_unique_violation())
                .unwrap_or(false);
            if unique {
                return Err(ChatError::Conflict);
            }
            return Err(e.into());
        }
        tx.commit().await?;
        Ok(ChatTurnRow {
            id: turn_id,
            session_id,
            user_seq: seq,
            status: ChatTurnStatus::Running,
            attempts: 0,
        })
    }

    /// Append an agent-produced row (tool / assistant / error): assign
    /// seq inside the tx, seal with it as AAD, bump `updated_at`.
    /// Returns the assigned seq.
    pub async fn append_message(
        &self,
        user_id: Uuid,
        master: &MasterKey,
        session_id: Uuid,
        role: ChatRole,
        content: &str,
    ) -> Result<i64> {
        let dek = ChatKeys::from_master_key(master).chat_dek().clone();
        let mut tx = self.pool.begin().await?;
        // Ownership gate (a foreign session id must be NotFound).
        let owned: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM chat_sessions WHERE id = ? AND user_id = ?")
                .bind(session_id.to_string())
                .bind(user_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if owned.is_none() {
            return Err(ChatError::NotFound);
        }
        let max_seq: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM chat_messages WHERE session_id = ?")
                .bind(session_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let seq = max_seq.unwrap_or(0) + 1;
        let sealed = seal_hex(content, &message_aad(session_id, seq), &dek)?;
        let ts = now();
        sqlx::query(
            "INSERT INTO chat_messages (session_id, seq, role, content_sealed, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id.to_string())
        .bind(seq)
        .bind(role.as_str())
        .bind(&sealed)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE chat_sessions SET updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(&ts)
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(seq)
    }

    /// Atomically resume an interrupted turn (resume worker input): the
    /// conditional UPDATE is the once-guard — exactly the first claimer
    /// wins (`attempts` 0 → 1). `Ok(false)` = already claimed, resumed
    /// a second time, not interrupted, or not owned.
    pub async fn claim_resumable(
        &self,
        user_id: Uuid,
        session_id: Uuid,
        turn_id: Uuid,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE chat_turns
             SET status = 'running', attempts = attempts + 1, updated_at = ?
             WHERE id = ? AND session_id = ?
               AND status = 'interrupted' AND attempts = 0
               AND session_id IN (SELECT id FROM chat_sessions WHERE id = ? AND user_id = ?)",
        )
        .bind(now())
        .bind(turn_id.to_string())
        .bind(session_id.to_string())
        .bind(session_id.to_string())
        .bind(user_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Transition a running turn to `status` (Done/Failed/Interrupted).
    ///
    /// Conditional on `status = 'running'` and idempotent: the Drop
    /// guard may fire after a normal finalize — a second transition
    /// finds no running row and no-ops. Ownership-scoped via a
    /// subquery (the guard holds the user id); `NotFound` when the turn
    /// is gone or never belonged to this user's session.
    pub async fn finalize_turn(
        &self,
        user_id: Uuid,
        turn_id: Uuid,
        status: ChatTurnStatus,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE chat_turns
             SET status = ?, updated_at = ?
             WHERE id = ?
               AND status = 'running'
               AND session_id IN (SELECT id FROM chat_sessions WHERE id = chat_turns.session_id AND user_id = ?)",
        )
        .bind(status.as_str())
        .bind(now())
        .bind(turn_id.to_string())
        .bind(user_id.to_string())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            // A finalized-already turn (idempotent no-op) is fine; an
            // unknown/foreign turn is NotFound. Distinguish by whether
            // the turn row exists under this user at all.
            let exists: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM chat_turns t
                 JOIN chat_sessions s ON s.id = t.session_id
                 WHERE t.id = ? AND s.user_id = ?",
            )
            .bind(turn_id.to_string())
            .bind(user_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            if exists.is_none() {
                return Err(ChatError::NotFound);
            }
        }
        Ok(())
    }

    /// Boot sweep: every `running` turn becomes `interrupted` (a dead
    /// process cannot finalize). Returns the number of turns swept.
    /// NOT user-scoped — the runner at boot iterates everything it owns.
    pub async fn boot_sweep(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE chat_turns SET status = 'interrupted', updated_at = ?
             WHERE status = 'running'",
        )
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Every resumable turn across all users (`interrupted`,
    /// `attempts = 0`), oldest first. NOT user-scoped — the resume
    /// worker iterates users; it recovers keys per user before claiming.
    pub async fn resumable_turns(&self) -> Result<Vec<(Uuid, ChatTurnRow)>> {
        let rows: Vec<(String, String, i64, String, i32, String)> = sqlx::query_as(
            "SELECT t.id, t.session_id, t.user_seq, t.status, t.attempts, s.user_id
             FROM chat_turns t
             JOIN chat_sessions s ON s.id = t.session_id
             WHERE t.status = 'interrupted' AND t.attempts = 0
             ORDER BY t.created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, session_id, user_seq, status, attempts, user_id)| {
                let user_id = Uuid::parse_str(&user_id)
                    .map_err(|_| sqlx::Error::Decode("bad chat session user_id".into()))?;
                let row = turn_row((id, session_id, user_seq, status, attempts))?;
                Ok((user_id, row))
            })
            .collect()
    }

    /// The session's running turn, if any (reattach needs the turn id
    /// to subscribe to the live event channel).
    pub async fn running_turn(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<ChatTurnRow>> {
        let rows: Vec<(String, String, i64, String, i32)> = sqlx::query_as(
            "SELECT t.id, t.session_id, t.user_seq, t.status, t.attempts
             FROM chat_turns t
             JOIN chat_sessions s ON s.id = t.session_id
             WHERE t.session_id = ? AND s.user_id = ? AND t.status = 'running'",
        )
        .bind(session_id.to_string())
        .bind(user_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        // The unique index admits at most one; take the first anyway.
        rows.into_iter().next().map(turn_row).transpose()
    }

    /// The session's newest turn by `created_at` (session_meta's turn
    /// fields derive from this).
    pub async fn latest_turn(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<ChatTurnRow>> {
        let rows: Vec<(String, String, i64, String, i32)> = sqlx::query_as(
            "SELECT t.id, t.session_id, t.user_seq, t.status, t.attempts
             FROM chat_turns t
             JOIN chat_sessions s ON s.id = t.session_id
             WHERE t.session_id = ? AND s.user_id = ?
             ORDER BY t.created_at DESC LIMIT 1",
        )
        .bind(session_id.to_string())
        .bind(user_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().next().map(turn_row).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_roundtrips_db_values() {
        for (v, role) in [
            ("user", ChatRole::User),
            ("tool", ChatRole::Tool),
            ("assistant", ChatRole::Assistant),
            ("error", ChatRole::Error),
        ] {
            assert_eq!(role.as_str(), v);
            assert_eq!(ChatRole::from_str(v), Some(role));
        }
        assert_eq!(ChatRole::from_str("other"), None);
    }

    #[test]
    fn status_roundtrips_db_values() {
        for (v, status) in [
            ("running", ChatTurnStatus::Running),
            ("done", ChatTurnStatus::Done),
            ("failed", ChatTurnStatus::Failed),
            ("interrupted", ChatTurnStatus::Interrupted),
            ("abandoned", ChatTurnStatus::Abandoned),
        ] {
            assert_eq!(status.as_str(), v);
            assert_eq!(ChatTurnStatus::from_str(v), Some(status));
        }
        assert_eq!(ChatTurnStatus::from_str("other"), None);
    }

    #[test]
    fn aad_formats() {
        let s = Uuid::new_v4();
        assert_eq!(message_aad(s, 7), format!("chat:{s}:7"));
        assert_eq!(title_aad(s), format!("chat:{s}:title"));
    }
}
