//! Storage models (row types for the SQLite schema).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub role: String,
    pub auth_provider: String,
    pub password_hash: Option<String>,
    pub sealed_master_key: String,
    pub must_change_password: bool,
    pub totp_secret: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub csrf_token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub user_id: Uuid,
    pub key_hash: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookshelf {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub is_global_read: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub id: Uuid,
    pub bookshelf_id: Uuid,
    pub slug: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IngestStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl IngestStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IngestStatus::Pending => "pending",
            IngestStatus::Running => "running",
            IngestStatus::Done => "done",
            IngestStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(IngestStatus::Pending),
            "running" => Some(IngestStatus::Running),
            "done" => Some(IngestStatus::Done),
            "failed" => Some(IngestStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestJob {
    pub id: Uuid,
    pub bookshelf_id: Uuid,
    pub book_id: Uuid,
    pub requested_by_user_id: Uuid,
    pub status: IngestStatus,
    pub detail: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The tool that enqueued a mutation-queue item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueTool {
    Add,
    Update,
    Maintain,
}

impl QueueTool {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Maintain => "maintain",
        }
    }

    // Deliberate inherent `from_str -> Option` (plan interface; mirrors the
    // `IngestStatus::parse`-style `Option` contract), not `std::str::FromStr`,
    // which would force a `Result` and diverge from the queue models' contract.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "add" => Some(Self::Add),
            "update" => Some(Self::Update),
            "maintain" => Some(Self::Maintain),
            _ => None,
        }
    }
}

/// A mutation-queue item's lifecycle. `staging` is internal (enqueue
/// phase); receipt views map it to `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueStatus {
    Staging,
    Pending,
    Running,
    Done,
    Dead,
}

impl QueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Dead => "dead",
        }
    }

    // Deliberate inherent `from_str -> Option` (plan interface; mirrors the
    // `IngestStatus::parse`-style `Option` contract), not `std::str::FromStr`,
    // which would force a `Result` and diverge from the queue models' contract.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "staging" => Some(Self::Staging),
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "dead" => Some(Self::Dead),
            _ => None,
        }
    }
}

/// A queued mutation item (metadata only — content lives in the
/// encrypted FileRepo payload at /mutation-queue/<id>).
#[derive(Debug, Clone)]
pub struct MutationQueueItem {
    pub id: Uuid,
    pub user_id: Uuid,
    pub tool: QueueTool,
    pub status: QueueStatus,
    pub attempts: u32,
    pub detail: String,
    pub created_at: String,
    pub updated_at: String,
    pub next_retry_at: Option<String>,
    pub final_paths: Option<String>,
}
