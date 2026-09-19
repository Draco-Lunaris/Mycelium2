//! Authentication and authorization.

pub mod api_key;
pub mod jwt;
pub mod login;
pub mod oidc;
pub mod password;
pub mod rbac;
pub mod session;
pub mod throttle;
pub mod totp;
pub mod users;
pub mod webauthn;

pub use api_key::{ApiKeyManager, ApiKeyRecord};
pub use jwt::{AccessTokenClaims, JwtError, JwtKeys};
pub use login::{LoginError, LoginService, LoginSuccess};
pub use password::{
    PasswordError, check_password_policy, check_password_policy_min, hash_password, verify_password,
};
pub use rbac::{Role, SessionUser};
pub use session::{SessionManager, SessionRecord};
pub use throttle::{AuthThrottle, ThrottleDecision};
pub use users::{UserRecord, UserStore, UsersError};

/// Minimum password length (DESIGN decision: 20).
pub const MIN_PASSWORD_LEN: usize = 20;
