//! RBAC roles and axum extractors.

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown role: {s}"))
    }
}

/// The authenticated user attached to a request (by session or API key
/// middleware in Phase 5/6; extractors here enforce RBAC).
#[derive(Debug, Clone)]
pub struct SessionUser {
    pub user_id: Uuid,
    pub username: String,
    pub role: Role,
}

/// Extractor: any authenticated user (generic over router state).
impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionUser>()
            .cloned()
            .ok_or((axum::http::StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

/// Extractor: an admin user only (403 for non-admins).
pub struct RequireAdmin;

impl<S: Send + Sync> FromRequestParts<S> for RequireAdmin {
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let user = parts
            .extensions
            .get::<SessionUser>()
            .cloned()
            .ok_or((axum::http::StatusCode::UNAUTHORIZED, "unauthorized"))?;
        if user.role != Role::Admin {
            return Err((axum::http::StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trip() {
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("user"), Some(Role::User));
        assert_eq!(Role::parse("other"), None);
        assert_eq!(Role::Admin.as_str(), "admin");
    }
}
