//! OIDC SSO: admin-configured provider (no defaults — public project),
//! authorize-URL generation and code exchange. Disabled unless configured.

use mycelium_store::ConfigStore;
use openidconnect::EndpointSet;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, RedirectUrl, TokenResponse,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("oidc is not configured")]
    NotConfigured,
    #[error("config store error: {0}")]
    Config(#[from] mycelium_store::ConfigError),
    #[error("oidc discovery failed: {0}")]
    Discovery(String),
    #[error("oidc protocol error: {0}")]
    Protocol(String),
}

/// Admin-managed OIDC provider configuration (stored in the config KV).
/// No defaults: every field is supplied by the admin in the portal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    /// Auto-provision local accounts for new OIDC identities (default true).
    #[serde(default = "default_true")]
    pub auto_provision: bool,
}

fn default_true() -> bool {
    true
}

const CONFIG_KEY: &str = "oidc";

/// Read the OIDC config (None when not configured).
pub async fn get_config(config: &ConfigStore) -> Result<Option<OidcConfig>, OidcError> {
    config
        .get::<OidcConfig>(CONFIG_KEY)
        .await
        .map_err(Into::into)
}

/// Save the OIDC config (admin portal action).
pub async fn set_config(config: &ConfigStore, cfg: &OidcConfig) -> Result<(), OidcError> {
    config.set(CONFIG_KEY, cfg).await.map_err(Into::into)
}

/// Clear the OIDC config (disable SSO).
pub async fn clear_config(config: &ConfigStore) -> Result<(), OidcError> {
    config.delete(CONFIG_KEY).await.map_err(Into::into)
}

/// A discovered OIDC client (built from the stored config). The type
/// parameters reflect the post-discovery endpoint states.
pub struct OidcClient {
    client: CoreClient<
        EndpointSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointMaybeSet,
        openidconnect::EndpointMaybeSet,
    >,
}

/// A nonce verifier that checks the id_token nonce against the expected
/// value (issued at authorize time, session-bound server-side).
pub struct ExpectedNonce(String);

impl openidconnect::NonceVerifier for ExpectedNonce {
    fn verify(self, nonce: Option<&Nonce>) -> Result<(), String> {
        let Some(claims_nonce) = nonce else {
            return Err("id_token has no nonce".into());
        };
        if claims_nonce.secret() != &self.0 {
            return Err("nonce mismatch".into());
        }
        Ok(())
    }
}

/// Discover the provider metadata and build a client.
/// Requires the config's issuer to be reachable (admin portal validates
/// before saving).
pub async fn build_client(cfg: &OidcConfig) -> Result<OidcClient, OidcError> {
    let issuer =
        IssuerUrl::new(cfg.issuer_url.clone()).map_err(|e| OidcError::Discovery(e.to_string()))?;
    // No redirects: following them opens SSRF vulnerabilities.
    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| OidcError::Discovery(e.to_string()))?;
    let provider = CoreProviderMetadata::discover_async(issuer, &http_client)
        .await
        .map_err(|e| OidcError::Discovery(e.to_string()))?;
    let client = CoreClient::from_provider_metadata(
        provider,
        ClientId::new(cfg.client_id.clone()),
        Some(ClientSecret::new(cfg.client_secret.clone())),
    )
    .set_redirect_uri(
        RedirectUrl::new(cfg.redirect_uri.clone())
            .map_err(|e| OidcError::Discovery(e.to_string()))?,
    );
    Ok(OidcClient { client })
}

/// The authorize redirect: URL + CSRF state + nonce (the caller stores the
/// nonce server-side, bound to the session).
pub struct AuthorizeRedirect {
    pub url: String,
    pub csrf_state: String,
    pub nonce: String,
}

impl OidcClient {
    /// Generate the authorization URL for the authorization-code flow.
    pub fn authorize_url(&self) -> AuthorizeRedirect {
        let (url, csrf, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .url();
        AuthorizeRedirect {
            url: url.to_string(),
            csrf_state: csrf.secret().to_string(),
            nonce: nonce.secret().to_string(),
        }
    }

    /// Exchange an authorization code for tokens.
    ///
    /// - `callback_state`: the `state` query parameter from the provider's
    ///   redirect (the caller passes it here for verification).
    /// - `expected_state`: the CSRF state issued by `authorize_url` and
    ///   stored server-side (session-bound).
    /// - `expected_nonce`: the nonce issued at authorize time; the id_token
    ///   nonce must match it.
    ///
    /// A state mismatch is rejected (login-CSRF defense).
    pub async fn exchange_code(
        &self,
        code: &str,
        callback_state: &str,
        expected_state: &str,
        expected_nonce: &str,
    ) -> Result<OidcIdentity, OidcError> {
        // Constant-time state comparison (login-CSRF defense).
        if !constant_time_eq(callback_state.as_bytes(), expected_state.as_bytes()) {
            return Err(OidcError::Protocol("state mismatch".into()));
        }
        let http_client = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OidcError::Protocol(e.to_string()))?;
        let token = self
            .client
            .exchange_code(openidconnect::AuthorizationCode::new(code.to_string()))
            .map_err(|e| OidcError::Protocol(e.to_string()))?
            .request_async(&http_client)
            .await
            .map_err(|e| OidcError::Protocol(e.to_string()))?;
        let id_token = token
            .id_token()
            .ok_or_else(|| OidcError::Protocol("no id_token in response".into()))?;
        let claims = id_token
            .claims(
                &self.client.id_token_verifier(),
                ExpectedNonce(expected_nonce.to_string()),
            )
            .map_err(|e| OidcError::Protocol(e.to_string()))?;
        let subject = claims.subject().to_string();
        let email = claims.email().map(|e| e.as_str().to_string());
        let preferred_username = claims.preferred_username().map(|u| u.as_str().to_string());
        Ok(OidcIdentity {
            subject,
            email,
            preferred_username,
        })
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

/// The external identity resolved from an OIDC login.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    /// Stable subject claim (unique per provider).
    pub subject: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    #[tokio::test]
    async fn config_round_trip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        // Not configured initially.
        assert!(get_config(&config).await.unwrap().is_none());
        // Save.
        let cfg = OidcConfig {
            issuer_url: "https://sso.example.com/realms/test".into(),
            client_id: "mycelium2".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://mycelium.example.com/auth/oidc/callback".into(),
            auto_provision: true,
        };
        set_config(&config, &cfg).await.unwrap();
        assert_eq!(get_config(&config).await.unwrap(), Some(cfg));
        // Clear.
        clear_config(&config).await.unwrap();
        assert!(get_config(&config).await.unwrap().is_none());
    }
}
