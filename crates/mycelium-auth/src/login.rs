//! Login service: the single entry point for authentication that wires
//! throttling, session lifecycle, and forced password changes together so
//! Phase 5 callers cannot forget them.

use uuid::Uuid;

use crate::rbac::Role;
use crate::session::{SessionManager, SessionRecord};
use crate::throttle::{AuthThrottle, ThrottleDecision};
use crate::users::{AuthenticatedUser, UserStore, UsersError};

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("too many attempts; retry after {retry_after_secs}s")]
    Throttled { retry_after_secs: u64 },
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("users error: {0}")]
    Users(#[from] UsersError),
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    #[error("TOTP error: {0}")]
    Totp(#[from] crate::totp::TotpError),
}

/// The result of a successful login.
pub struct LoginSuccess {
    pub user: AuthenticatedUser,
    pub session: SessionRecord,
    /// True when the user must change their password before doing anything
    /// else (bootstrap admin). Phase 5 handlers MUST gate on this.
    pub must_change_password: bool,
}

/// Owns the full login flow: throttle → authenticate → TOTP → session.
/// The throttle uses interior mutability so `login` takes `&self` (safe
/// to share across handlers).
#[derive(Debug, Clone)]
pub struct LoginService {
    users: UserStore,
    sessions: SessionManager,
    throttle: std::sync::Arc<std::sync::Mutex<AuthThrottle>>,
}

/// Per-call runtime security settings (admin-configurable via the web
/// layer's ConfigStore). `None` fields keep the built-in defaults.
/// All values are expected pre-clamped by the caller.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    /// Session TTL override.
    pub session_ttl: Option<chrono::Duration>,
    /// Throttle: failures before the first deny.
    pub login_max_failures: Option<u32>,
    /// Throttle: backoff cap.
    pub login_lockout: Option<std::time::Duration>,
    /// Password policy: minimum length (applies to password change).
    pub min_password_length: Option<usize>,
}

impl LoginService {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self {
            users: UserStore::new(pool.clone()),
            sessions: SessionManager::new(pool),
            throttle: std::sync::Arc::new(std::sync::Mutex::new(AuthThrottle::new())),
        }
    }

    /// Log a user in with username + password (+ optional TOTP code),
    /// with the built-in security defaults.
    ///
    /// - Throttled per username with exponential backoff (recorded on
    ///   failure, reset on success).
    /// - TOTP is checked when the user has a secret enrolled (pass None to
    ///   skip only if unenrolled; enrolled users must supply a code).
    /// - A fresh session is always minted (no fixation).
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        totp_code: Option<&str>,
    ) -> Result<LoginSuccess, LoginError> {
        self.login_with_options(username, password, totp_code, &LoginOptions::default())
            .await
    }

    /// Log a user in with runtime security settings (admin-configurable;
    /// the web layer passes its ConfigStore values, pre-clamped).
    pub async fn login_with_options(
        &self,
        username: &str,
        password: &str,
        totp_code: Option<&str>,
        options: &LoginOptions,
    ) -> Result<LoginSuccess, LoginError> {
        // Apply the runtime throttle limits (if configured) before the
        // check — the throttle is shared state, so this affects all
        // concurrent logins too. Idempotent: setting the same limits
        // repeatedly is a no-op.
        {
            let mut throttle = self.throttle.lock().unwrap();
            if let (Some(max_f), Some(lockout)) =
                (options.login_max_failures, options.login_lockout)
            {
                throttle.set_limits(max_f, lockout);
            }
        }
        // Throttle check (per username; Phase 5 should also key by IP).
        let decision = self.throttle.lock().unwrap().check(username);
        if let ThrottleDecision::Deny { retry_after } = decision {
            return Err(LoginError::Throttled {
                retry_after_secs: retry_after.as_secs().max(1),
            });
        }
        // Authenticate.
        let auth = match self.users.authenticate_local(username, password).await {
            Ok(auth) => auth,
            Err(UsersError::InvalidCredentials | UsersError::NotFound) => {
                self.throttle.lock().unwrap().record_failure(username);
                // Uniform error: no user-enumeration distinction.
                return Err(LoginError::InvalidCredentials);
            }
            Err(e) => return Err(e.into()),
        };
        // TOTP (enrolled users must pass a code).
        if let Some(secret) = &auth.record.totp_secret {
            let code = totp_code.ok_or(LoginError::InvalidCredentials)?;
            let totp = crate::totp::make_totp_for_verify(secret)?;
            if !totp.check_current(code).is_some_and(|t| t > 0) {
                self.throttle.lock().unwrap().record_failure(username);
                return Err(LoginError::InvalidCredentials);
            }
        }
        // Success: reset throttle, mint a fresh session.
        self.throttle.lock().unwrap().record_success(username);
        let session = match options.session_ttl {
            Some(ttl) => self.sessions.create_with_ttl(auth.record.id, ttl).await?,
            None => self.sessions.create(auth.record.id).await?,
        };
        Ok(LoginSuccess {
            must_change_password: auth.record.must_change_password,
            user: auth,
            session,
        })
    }

    /// Change password AND invalidate all existing sessions (the stolen-
    /// session defense). Requires the old password. Uses the default
    /// password policy.
    pub async fn change_password(
        &self,
        user_id: Uuid,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), LoginError> {
        self.change_password_min(user_id, old_password, new_password, crate::MIN_PASSWORD_LEN)
            .await
    }

    /// Change password with an explicit minimum length (admin-configurable
    /// policy; callers clamp to sane bounds) AND invalidate all sessions.
    pub async fn change_password_min(
        &self,
        user_id: Uuid,
        old_password: &str,
        new_password: &str,
        min_len: usize,
    ) -> Result<(), LoginError> {
        self.users
            .change_password_min(user_id, old_password, new_password, min_len)
            .await?;
        // Invalidate every session for this user.
        self.sessions.delete_all_for_user(user_id).await?;
        Ok(())
    }

    /// Log out (delete one session).
    pub async fn logout(&self, session_id: Uuid) -> Result<(), LoginError> {
        self.sessions.delete(session_id).await?;
        Ok(())
    }

    /// Create a user (admin action) — convenience passthrough.
    pub async fn create_user(
        &self,
        username: &str,
        email: &str,
        password: &str,
        role: Role,
    ) -> Result<crate::users::CreatedUser, LoginError> {
        Ok(self
            .users
            .create_local(username, email, password, role)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    const PW: &str = "correct horse battery staple";

    #[tokio::test]
    async fn login_success_mints_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let svc = LoginService::new(store.pool().clone());
        svc.create_user("alice", "a@t", PW, Role::User)
            .await
            .unwrap();
        let success = svc.login("alice", PW, None).await.unwrap();
        assert!(!success.must_change_password);
        assert_eq!(success.user.record.username, "alice");
        // Session is live.
        assert!(store.pool().acquire().await.is_ok());
    }

    #[tokio::test]
    async fn wrong_password_throttles() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let svc = LoginService::new(store.pool().clone());
        svc.create_user("alice", "a@t", PW, Role::User)
            .await
            .unwrap();
        for _ in 0..3 {
            let _ = svc.login("alice", "wrong password entirely!!", None).await;
        }
        // 4th attempt (even with the right password) is throttled.
        assert!(matches!(
            svc.login("alice", PW, None).await,
            Err(LoginError::Throttled { .. })
        ));
    }

    #[tokio::test]
    async fn unknown_user_uniform_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let svc = LoginService::new(store.pool().clone());
        assert!(matches!(
            svc.login("ghost", "whatever password here!!", None).await,
            Err(LoginError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn change_password_invalidates_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let svc = LoginService::new(store.pool().clone());
        let created = svc
            .create_user("alice", "a@t", PW, Role::User)
            .await
            .unwrap();
        let s1 = svc.login("alice", PW, None).await.unwrap();
        let s2 = svc.login("alice", PW, None).await.unwrap();
        // Change password: both sessions die.
        svc.change_password(created.record.id, PW, "new password 20 chars!")
            .await
            .unwrap();
        let sessions = SessionManager::new(store.pool().clone());
        assert!(sessions.get(s1.session.id).await.is_err());
        assert!(sessions.get(s2.session.id).await.is_err());
        // New password logs in.
        let _ = svc
            .login("alice", "new password 20 chars!", None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn totp_enrolled_requires_code() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let svc = LoginService::new(store.pool().clone());
        let created = svc
            .create_user("alice", "a@t", PW, Role::User)
            .await
            .unwrap();
        // Enroll TOTP.
        let secret = crate::totp::generate_secret();
        crate::totp::set_secret(store.pool(), created.record.id, &secret)
            .await
            .unwrap();
        // Login without a code fails.
        assert!(matches!(
            svc.login("alice", PW, None).await,
            Err(LoginError::InvalidCredentials)
        ));
        // Login with a valid code succeeds.
        let totp = crate::totp::make_totp_for_verify(&secret).unwrap();
        let code = totp.generate_current().to_string();
        let ok = svc.login("alice", PW, Some(&code)).await.unwrap();
        assert_eq!(ok.user.record.username, "alice");
    }
}
