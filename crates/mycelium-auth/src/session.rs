//! Server-side sessions in SQLite with CSRF tokens.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("session not found or expired")]
    NotFound,
}

/// A live session row.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
}

/// Session lifecycle over the store pool.
#[derive(Debug, Clone)]
pub struct SessionManager {
    pool: sqlx::SqlitePool,
    ttl: Duration,
}

impl SessionManager {
    /// Default web session TTL: 12 hours.
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self {
            pool,
            ttl: Duration::hours(12),
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Create a session for a user. Returns the record; the caller sets the
    /// httpOnly cookie with the session id and keeps the CSRF token for the
    /// double-submit pattern.
    pub async fn create(&self, user_id: Uuid) -> Result<SessionRecord, SessionError> {
        let id = Uuid::new_v4();
        let csrf = format!("myc2-csrf-{}", Uuid::new_v4());
        let now = Utc::now();
        let expires = now + self.ttl;
        sqlx::query(
            "INSERT INTO sessions (id, user_id, csrf_token, created_at, expires_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(&csrf)
        .bind(now.to_rfc3339())
        .bind(expires.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(SessionRecord {
            id,
            user_id,
            csrf_token: csrf,
            expires_at: expires,
        })
    }

    /// Look up a session; expired sessions are deleted and reported absent.
    pub async fn get(&self, session_id: Uuid) -> Result<SessionRecord, SessionError> {
        let row: Option<(String, String, String, String)> =
            sqlx::query_as("SELECT id, user_id, csrf_token, expires_at FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let Some((id_s, user_s, csrf, expires_s)) = row else {
            return Err(SessionError::NotFound);
        };
        let expires = DateTime::parse_from_rfc3339(&expires_s)
            .map_err(|_| SessionError::NotFound)?
            .with_timezone(&Utc);
        if Utc::now() > expires {
            // Expired: delete and report absent.
            let _ = sqlx::query("DELETE FROM sessions WHERE id = ?")
                .bind(&id_s)
                .execute(&self.pool)
                .await;
            return Err(SessionError::NotFound);
        }
        Ok(SessionRecord {
            id: Uuid::parse_str(&id_s).map_err(|_| SessionError::NotFound)?,
            user_id: Uuid::parse_str(&user_s).map_err(|_| SessionError::NotFound)?,
            csrf_token: csrf,
            expires_at: expires,
        })
    }

    /// Delete a session (logout).
    pub async fn delete(&self, session_id: Uuid) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete all sessions for a user (password change, admin force-logout).
    pub async fn delete_all_for_user(&self, user_id: Uuid) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Purge all expired sessions (maintenance).
    pub async fn purge_expired(&self) -> Result<u64, SessionError> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
            .bind(&now)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Verify a CSRF token against a session's token (constant-time).
    /// For the double-submit pattern: the cookie carries the session id,
    /// the form/header carries the CSRF token.
    pub async fn verify_csrf(
        &self,
        session_id: Uuid,
        presented_token: &str,
    ) -> Result<bool, SessionError> {
        let session = self.get(session_id).await?;
        Ok(constant_time_eq(
            presented_token.as_bytes(),
            session.csrf_token.as_bytes(),
        ))
    }
}

/// Constant-time byte-slice equality (length differences leak only length).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    /// Insert a minimal user row so FK constraints are satisfied.
    async fn seed_user(pool: &sqlx::SqlitePool) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at)
             VALUES (?, ?, ?, 'user', 'local', '{}', ?, ?)",
        )
        .bind(id.to_string())
        .bind(format!("u-{id}"))
        .bind(format!("u-{id}@t"))
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn create_get_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let sessions = SessionManager::new(store.pool().clone());
        let user_id = seed_user(store.pool()).await;
        let s = sessions.create(user_id).await.unwrap();
        assert!(s.csrf_token.starts_with("myc2-csrf-"));
        let got = sessions.get(s.id).await.unwrap();
        assert_eq!(got.user_id, user_id);
        sessions.delete(s.id).await.unwrap();
        assert!(matches!(
            sessions.get(s.id).await,
            Err(SessionError::NotFound)
        ));
    }

    #[tokio::test]
    async fn expired_session_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let sessions =
            SessionManager::new(store.pool().clone()).with_ttl(Duration::milliseconds(1));
        let user_id = seed_user(store.pool()).await;
        let s = sessions.create(user_id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(matches!(
            sessions.get(s.id).await,
            Err(SessionError::NotFound)
        ));
    }

    #[tokio::test]
    async fn delete_all_for_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let sessions = SessionManager::new(store.pool().clone());
        let user_id = seed_user(store.pool()).await;
        let a = sessions.create(user_id).await.unwrap();
        let b = sessions.create(user_id).await.unwrap();
        sessions.delete_all_for_user(user_id).await.unwrap();
        assert!(sessions.get(a.id).await.is_err());
        assert!(sessions.get(b.id).await.is_err());
    }

    #[tokio::test]
    async fn csrf_verification() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let sessions = SessionManager::new(store.pool().clone());
        let user_id = seed_user(store.pool()).await;
        let s = sessions.create(user_id).await.unwrap();
        // Correct token passes.
        assert!(sessions.verify_csrf(s.id, &s.csrf_token).await.unwrap());
        // Wrong token fails.
        assert!(!sessions.verify_csrf(s.id, "myc2-csrf-wrong").await.unwrap());
        // Unknown session errors.
        assert!(sessions.verify_csrf(Uuid::new_v4(), "x").await.is_err());
    }
}
