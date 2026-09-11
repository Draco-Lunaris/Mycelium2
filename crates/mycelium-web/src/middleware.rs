//! Middleware: session authentication, CSRF double-submit, CSP nonce +
//! security headers.

use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::state::AppState;

/// The session cookie name.
pub const SESSION_COOKIE: &str = "myc2_session";

/// Extract the session id from the request's cookies.
pub fn session_id_from_headers(headers: &axum::http::HeaderMap) -> Option<Uuid> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookies.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("myc2_session=") {
            return Uuid::parse_str(value).ok();
        }
    }
    None
}

/// Session-auth middleware: resolves the session cookie to a user and
/// injects `SessionUser` into request extensions. Public paths are
/// allowed through without a session.
pub async fn session_auth(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    crate::state::Metrics::inc(&state.metrics.requests_total);
    if let Some(session_id) = session_id_from_headers(request.headers())
        && let Ok(session) = state.sessions.get(session_id).await
        && let Ok(user) = state.users.get_by_id(session.user_id).await
    {
        request
            .extensions_mut()
            .insert(mycelium_auth::rbac::SessionUser {
                user_id: user.id,
                username: user.username.clone(),
                role: user.role,
            });
        // Stash the session id for logout/CSRF checks.
        request.extensions_mut().insert(SessionId(session_id));
    }
    // Bearer API-key auth (MCP + programmatic clients): the token is not
    // ambient (browsers never attach it), so CSRF does not apply. The
    // /mcp endpoint verifies its own bearer tokens (the MCP router's
    // middleware) — skip here to avoid double verification.
    if request
        .extensions()
        .get::<mycelium_auth::rbac::SessionUser>()
        .is_none()
        && request.uri().path() != "/mcp"
        && let Some(user) = bearer_user(&state, request.headers()).await
    {
        request.extensions_mut().insert(user);
        request.extensions_mut().insert(BearerAuth);
    }
    next.run(request).await
}

/// Marker: the request authenticated via a bearer API key (CSRF-exempt).
#[derive(Debug, Clone, Copy)]
pub struct BearerAuth;

/// Resolve an `Authorization: Bearer myc2-...` API key to its user.
async fn bearer_user(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Option<mycelium_auth::rbac::SessionUser> {
    let auth = headers.get(axum::http::header::AUTHORIZATION)?;
    let auth = auth.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    let manager = mycelium_auth::api_key::ApiKeyManager::new(state.store.pool().clone());
    let record = match manager.verify(token).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("bearer verify failed: {e}");
            return None;
        }
    };
    let user = state.users.get_by_id(record.user_id).await.ok()?;
    Some(mycelium_auth::rbac::SessionUser {
        user_id: user.id,
        username: user.username,
        role: user.role,
    })
}

/// The resolved session id (injected by session_auth). Extractable in
/// handlers (401 when absent).
#[derive(Debug, Clone, Copy)]
pub struct SessionId(pub Uuid);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for SessionId {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionId>()
            .copied()
            .ok_or((StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

/// CSRF double-submit verification for state-changing requests (POST/PUT/
/// DELETE). The token is accepted from the `x-csrf-token` header (fetch/
/// API clients) OR the `csrf_token` form field (native browser forms —
/// app.js injects it on submit; forms cannot set custom headers).
pub async fn csrf_protect(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    if method == axum::http::Method::GET
        || method == axum::http::Method::HEAD
        || method == axum::http::Method::OPTIONS
    {
        return next.run(request).await;
    }
    // The MCP endpoint authenticates via bearer API keys (not ambient
    // cookies), so CSRF does not apply; its own middleware enforces auth.
    if request.uri().path() == "/mcp" {
        return next.run(request).await;
    }
    // Bearer API-key requests are CSRF-exempt: the token is not ambient
    // (browsers never attach Authorization headers automatically), so
    // cross-site request forgery does not apply.
    if request.extensions().get::<BearerAuth>().is_some() {
        return next.run(request).await;
    }
    let Some(session) = request.extensions().get::<SessionId>().copied() else {
        // No session: only the login endpoint may proceed (it has no
        // session to protect yet).
        if request.uri().path() == "/login" {
            return next.run(request).await;
        }
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };

    let (mut parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_form = content_type.starts_with("application/x-www-form-urlencoded");
    let is_multipart = content_type.starts_with("multipart/form-data");

    let presented = parts
        .headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let presented = match presented {
        Some(token) => Some(token),
        None if is_form => {
            // Native form: buffer the body, extract csrf_token, restore.
            match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
                Ok(bytes) => {
                    let form: Option<String> = form_urlencoded_get(&bytes, "csrf_token");
                    // Restore the body.
                    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                    let restored = axum::http::Request::from_parts(
                        parts.clone(),
                        axum::body::Body::from(bytes.clone()),
                    );
                    // Stash for the session check below; re-run with body.
                    let token = form;
                    // Verify.
                    match token {
                        Some(token) => {
                            return verify_and_continue(state, session, token, restored, next)
                                .await;
                        }
                        None => {
                            return (StatusCode::FORBIDDEN, "missing CSRF token").into_response();
                        }
                    }
                }
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "body read failed").into_response();
                }
            }
        }
        None if is_multipart => {
            // Multipart upload (book ingest): buffer the body (uploads are
            // legitimately large — 33 MiB cap), extract the csrf_token
            // field with a boundary-aware parse, verify, restore.
            match axum::body::to_bytes(body, 33 * 1024 * 1024).await {
                Ok(bytes) => {
                    let token = multipart_field(&bytes, content_type, "csrf_token");
                    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                    let restored = axum::http::Request::from_parts(
                        parts.clone(),
                        axum::body::Body::from(bytes.clone()),
                    );
                    match token {
                        Some(token) => {
                            return verify_and_continue(state, session, token, restored, next)
                                .await;
                        }
                        None => {
                            return (StatusCode::FORBIDDEN, "missing CSRF token").into_response();
                        }
                    }
                }
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "body read failed").into_response();
                }
            }
        }
        None => None,
    };

    match presented {
        Some(token) => {
            let restored = axum::http::Request::from_parts(parts, body);
            verify_and_continue(state, session, token, restored, next).await
        }
        None => (StatusCode::FORBIDDEN, "missing CSRF token").into_response(),
    }
}

/// Extract a field's value from a buffered multipart/form-data body.
/// Boundary-aware but minimal: finds the named part and returns its
/// body bytes decoded as UTF-8 (lossy). Returns None when the field is
/// absent or the body is malformed.
fn multipart_field(body: &[u8], content_type: &str, field: &str) -> Option<String> {
    // boundary="..." or boundary=...
    let idx = content_type.find("boundary=")?;
    let raw = &content_type[idx + "boundary=".len()..];
    let boundary = raw
        .strip_prefix('"')
        .and_then(|r| r.split_once('"').map(|(b, _)| b))
        .unwrap_or_else(|| raw.split(';').next().unwrap_or(raw));
    if boundary.is_empty() {
        return None;
    }
    let delim = format!("--{boundary}");
    let text = body;
    let mut pos = 0usize;
    while let Some(start) = find_subsequence(&text[pos..], delim.as_bytes()) {
        let abs = pos + start + delim.len();
        // Part header runs to a blank line; body runs to the next delimiter.
        let header_end_rel = find_subsequence(&text[abs..], b"\r\n\r\n")?;
        let headers = String::from_utf8_lossy(&text[abs..abs + header_end_rel]).to_string();
        let body_start = abs + header_end_rel + 4;
        let next_delim = find_subsequence(&text[body_start..], delim.as_bytes())?;
        // Trim the trailing CRLF before the boundary.
        let mut body_end = body_start + next_delim;
        if body_end >= 2 && &text[body_end - 2..body_end] == b"\r\n" {
            body_end -= 2;
        }
        if headers
            .to_lowercase()
            .contains(&format!("name=\"{field}\"").to_lowercase())
        {
            return Some(String::from_utf8_lossy(&text[body_start..body_end]).to_string());
        }
        pos = body_start;
    }
    None
}

/// Find a subsequence in a byte slice.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Verify the presented token against the session and continue.
async fn verify_and_continue(
    state: AppState,
    session: SessionId,
    token: String,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    match state.sessions.verify_csrf(session.0, &token).await {
        Ok(true) => next.run(request).await,
        Ok(false) => (StatusCode::FORBIDDEN, "CSRF token mismatch").into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, "invalid session").into_response(),
    }
}

/// Extract a field from a urlencoded body without consuming a framework
/// extractor (percent-decoding included).
fn form_urlencoded_get(body: &[u8], field: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    for pair in text.split('&') {
        let (k, v) = pair.split_once('=')?;
        if percent_decode(k) == field {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Minimal percent-decoding for form values (+ as space, %XX hex).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                    16,
                ) {
                    out.push(hex);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Security headers + CSP with a per-response nonce. The nonce is embedded
/// in the CSP header and available to handlers via the response; inline
/// scripts (Leptos hydration) must carry `nonce="{nonce}"`.
pub async fn security_headers(request: Request<axum::body::Body>, next: Next) -> Response {
    let nonce = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(24)
        .collect::<String>()
        .to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let csp = format!(
        "default-src 'self'; script-src 'self' 'nonce-{nonce}'; \
         style-src 'self'; img-src 'self' data:; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp).expect("valid CSP"),
    );
    headers.insert(
        header::HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static("max-age=63072000; includeSubDomains"),
    );
    response
}
