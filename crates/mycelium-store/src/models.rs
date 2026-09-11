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
