//! OAuth2 device code authentication against tado° (`login.tado.com`).
//!
//! tado° removed the password grant on 2025-03-21. The device code grant requires a
//! one-time browser approval. After that, the rotating refresh token is persisted
//! to disk so the exporter can keep renewing access tokens unattended.

use std::fmt;
use std::time::{Duration, Instant};

use log::{info, warn};
use reqwest::StatusCode;
use serde_derive::Deserialize;

use super::token_store::TokenStore;

/// Public client id of the tado° web app, documented by tado° for the device code flow.
pub const DEFAULT_CLIENT_ID: &str = "1bb50063-6b0c-4d11-bd99-387f4a91cc46";
const DEFAULT_AUTH_BASE_URL: &str = "https://login.tado.com/oauth2/";
const SCOPE: &str = "offline_access";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_TOKEN_GRANT: &str = "refresh_token";
const INVALID_GRANT: &str = "invalid_grant";
/// Renew access tokens slightly before they expire to avoid racing the expiry.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// RFC 8628 §3.5: on `slow_down`, increase the polling interval by 5 seconds.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum AuthError {
    Http(reqwest::Error),
    /// The authorization server rejected the request with an OAuth error code.
    Rejected(String),
    DeviceCodeExpired,
    AccessDenied,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Http(e) => {
                write!(f, "HTTP error: {}", e)?;
                // reqwest hides the root cause (DNS, TLS, timeout, ...) in the source chain.
                let mut source = std::error::Error::source(e);
                while let Some(cause) = source {
                    write!(f, ": {}", cause)?;
                    source = cause.source();
                }
                Ok(())
            }
            AuthError::Rejected(code) => write!(f, "rejected by tado° ({})", code),
            AuthError::DeviceCodeExpired => {
                write!(f, "device authorization expired before it was approved")
            }
            AuthError::AccessDenied => write!(f, "device authorization was denied"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<reqwest::Error> for AuthError {
    fn from(e: reqwest::Error) -> Self {
        AuthError::Http(e)
    }
}

#[derive(Deserialize, Debug)]
struct DeviceAuthorizeResponse {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize, Debug)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: String,
}

#[derive(Deserialize, Debug)]
struct OAuthErrorResponse {
    error: String,
}

struct AccessToken {
    value: String,
    renew_at: Instant,
}

impl AccessToken {
    fn from_response(response: &TokenResponse) -> AccessToken {
        let lifetime = Duration::from_secs(response.expires_in).saturating_sub(EXPIRY_MARGIN);
        AccessToken {
            value: response.access_token.clone(),
            renew_at: Instant::now() + lifetime,
        }
    }

    fn is_valid(&self) -> bool {
        Instant::now() < self.renew_at
    }
}

pub struct Authenticator {
    http_client: reqwest::Client,
    base_url: reqwest::Url,
    client_id: String,
    store: TokenStore,
    refresh_token: Option<String>,
    access_token: Option<AccessToken>,
}

impl Authenticator {
    pub fn new(http_client: reqwest::Client, store: TokenStore) -> Authenticator {
        let base_url = DEFAULT_AUTH_BASE_URL
            .parse()
            .expect("default auth URL is valid");
        Authenticator::with_base_url(http_client, base_url, store)
    }

    pub(crate) fn with_base_url(
        http_client: reqwest::Client,
        base_url: reqwest::Url,
        store: TokenStore,
    ) -> Authenticator {
        let refresh_token = store.load().unwrap_or_else(|e| {
            warn!("unable to read stored tado° token: {}", e);
            None
        });

        Authenticator {
            http_client,
            base_url,
            client_id: DEFAULT_CLIENT_ID.to_string(),
            store,
            refresh_token,
            access_token: None,
        }
    }

    /// Overrides the OAuth client id (`EXPORTER_CLIENT_ID`).
    pub fn with_client_id(self, client_id: String) -> Authenticator {
        Authenticator { client_id, ..self }
    }

    /// Returns a valid access token. If needed, it is renewed with the refresh token,
    /// or a new device authorization is started.
    pub async fn access_token(&mut self) -> Result<String, AuthError> {
        if let Some(token) = self.access_token.as_ref().filter(|t| t.is_valid()) {
            return Ok(token.value.clone());
        }

        let response = self.obtain_token().await?;
        self.remember(&response);

        Ok(response.access_token)
    }

    /// Forgets the cached access token, e.g. after the API answered 401.
    pub fn invalidate(&mut self) {
        self.access_token = None;
    }

    async fn obtain_token(&self) -> Result<TokenResponse, AuthError> {
        if let Some(refresh_token) = &self.refresh_token {
            match self.refresh(refresh_token).await {
                // RFC 6749 §5.2: an invalid, expired or revoked refresh token yields `invalid_grant`.
                Err(AuthError::Rejected(code)) if code == INVALID_GRANT => {
                    warn!(
                        "stored tado° refresh token was rejected ({}), a new device authorization is required",
                        code
                    );
                }
                // Any other failure leaves the refresh token untouched: retry next tick.
                result => return result,
            }
        }

        self.authorize_device().await
    }

    fn remember(&mut self, response: &TokenResponse) {
        // Refresh tokens rotate: the previous one is revoked as soon as a new one is issued,
        // so the newest token must be kept in memory even if persisting it fails.
        if let Err(e) = self.store.save(&response.refresh_token) {
            warn!(
                "unable to persist tado° refresh token, a new device authorization will be needed after restart: {}",
                e
            );
        }
        self.refresh_token = Some(response.refresh_token.clone());
        self.access_token = Some(AccessToken::from_response(response));
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, AuthError> {
        self.request_token(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", REFRESH_TOKEN_GRANT),
            ("refresh_token", refresh_token),
        ])
        .await
    }

    async fn authorize_device(&self) -> Result<TokenResponse, AuthError> {
        let device = self.start_device_authorization().await?;

        warn!(
            "tado° authorization required: open {} in a browser and confirm code {} (valid for {}s)",
            device.verification_uri_complete, device.user_code, device.expires_in
        );

        let deadline = Instant::now() + Duration::from_secs(device.expires_in);
        let mut interval = Duration::from_secs(device.interval);

        loop {
            if Instant::now() >= deadline {
                return Err(AuthError::DeviceCodeExpired);
            }
            tokio::time::sleep(interval).await;

            let result = self
                .request_token(&[
                    ("client_id", self.client_id.as_str()),
                    ("device_code", device.device_code.as_str()),
                    ("grant_type", DEVICE_CODE_GRANT),
                ])
                .await;

            match result {
                Ok(response) => {
                    info!("tado° device authorization granted");
                    return Ok(response);
                }
                Err(AuthError::Rejected(code)) => match code.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => interval += SLOW_DOWN_STEP,
                    "access_denied" => return Err(AuthError::AccessDenied),
                    "expired_token" => return Err(AuthError::DeviceCodeExpired),
                    _ => return Err(AuthError::Rejected(code)),
                },
                // Dropped keep-alive connections, timeouts and 5xx must not abort the
                // authorization the user may be approving right now: retry until the deadline.
                Err(e @ AuthError::Http(_)) => {
                    warn!(
                        "transient error while waiting for tado° authorization, retrying: {}",
                        e
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn start_device_authorization(&self) -> Result<DeviceAuthorizeResponse, AuthError> {
        let url = self.endpoint("device_authorize");
        let response = self
            .http_client
            .post(url)
            .query(&[("client_id", self.client_id.as_str()), ("scope", SCOPE)])
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json::<DeviceAuthorizeResponse>().await?)
    }

    async fn request_token(&self, params: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        // tado° documents the OAuth parameters as query parameters, not a form body.
        let response = self
            .http_client
            .post(self.endpoint("token"))
            .query(params)
            .send()
            .await?;

        let status = response.status();
        let is_retryable =
            status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::REQUEST_TIMEOUT;

        if status.is_client_error() && !is_retryable {
            let http_error = response
                .error_for_status_ref()
                .expect_err("client error status");
            // Only a proper OAuth error body counts as a rejection; anything else is transient.
            return match response.json::<OAuthErrorResponse>().await {
                Ok(body) => Err(AuthError::Rejected(body.error)),
                Err(_) => Err(AuthError::Http(http_error)),
            };
        }

        Ok(response.error_for_status()?.json::<TokenResponse>().await?)
    }

    fn endpoint(&self, name: &str) -> reqwest::Url {
        self.base_url
            .join(name)
            .expect("auth endpoint names are valid URL paths")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

    const AUTH_PATH: &str = "/oauth2/";

    fn token_body(access: &str, refresh: &str) -> serde_json::Value {
        json!({
            "access_token": access,
            "expires_in": 599,
            "refresh_token": refresh,
            "scope": "offline_access",
            "token_type": "Bearer",
            "userId": "user-1"
        })
    }

    fn error_body(code: &str) -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(json!({ "error": code }))
    }

    async fn mount_device_authorize(server: &MockServer, expected_calls: u64) {
        Mock::given(method("POST"))
            .and(path("/oauth2/device_authorize"))
            .and(query_param("client_id", DEFAULT_CLIENT_ID))
            .and(query_param("scope", "offline_access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": "device-123",
                "user_code": "7BQ5ZQ",
                "verification_uri": "https://login.tado.com/oauth2/device",
                "verification_uri_complete": "https://login.tado.com/oauth2/device?user_code=7BQ5ZQ",
                "expires_in": 300,
                "interval": 0
            })))
            .expect(expected_calls)
            .mount(server)
            .await;
    }

    fn device_token_mock() -> MockBuilder {
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(query_param("grant_type", DEVICE_CODE_GRANT))
            .and(query_param("device_code", "device-123"))
            .and(query_param("client_id", DEFAULT_CLIENT_ID))
    }

    fn refresh_token_mock(refresh_token: &str) -> MockBuilder {
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(query_param("grant_type", "refresh_token"))
            .and(query_param("refresh_token", refresh_token))
            .and(query_param("client_id", DEFAULT_CLIENT_ID))
    }

    fn authenticator(server: &MockServer, dir: &TempDir) -> Authenticator {
        let base_url = format!("{}{}", server.uri(), AUTH_PATH).parse().unwrap();
        let store = TokenStore::new(dir.path().join("token.json"));
        Authenticator::with_base_url(reqwest::Client::new(), base_url, store)
    }

    fn stored_token(dir: &TempDir) -> Option<String> {
        TokenStore::new(dir.path().join("token.json"))
            .load()
            .unwrap()
    }

    #[tokio::test]
    async fn refreshes_with_stored_token_and_persists_rotated_token() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        refresh_token_mock("refresh-old")
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-new")),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_device_authorize(&server, 0).await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-new"));
    }

    #[tokio::test]
    async fn reuses_cached_access_token_until_it_expires() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        refresh_token_mock("refresh-old")
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-new")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let first = auth.access_token().await.unwrap();
        let second = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(first, "access-1");
        assert_eq!(second, "access-1");
    }

    #[tokio::test]
    async fn uses_rotated_refresh_token_after_invalidation() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-1")
            .unwrap();
        refresh_token_mock("refresh-1")
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-2")),
            )
            .expect(1)
            .mount(&server)
            .await;
        refresh_token_mock("refresh-2")
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-2", "refresh-3")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        auth.access_token().await.unwrap();
        auth.invalidate();
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-2");
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-3"));
    }

    #[tokio::test]
    async fn runs_device_flow_when_no_token_is_stored() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(error_body("authorization_pending"))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        device_token_mock()
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-1")),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn device_flow_survives_transient_polling_errors() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        device_token_mock()
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-1")),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
    }

    #[tokio::test]
    async fn falls_back_to_device_flow_when_refresh_token_is_rejected() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-revoked")
            .unwrap();
        refresh_token_mock("refresh-revoked")
            .respond_with(error_body("invalid_grant"))
            .expect(1)
            .mount(&server)
            .await;
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-1")),
            )
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn sends_configured_client_id() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(query_param("client_id", "custom-client"))
            .and(query_param("refresh_token", "refresh-old"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-new")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir).with_client_id("custom-client".to_string());

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
    }

    #[tokio::test]
    async fn keeps_refresh_token_when_server_errors() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        refresh_token_mock("refresh-old")
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        mount_device_authorize(&server, 0).await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let result = auth.access_token().await;

        // Assert
        assert!(matches!(result, Err(AuthError::Http(_))));
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-old"));
    }

    #[tokio::test]
    async fn keeps_refresh_token_when_rate_limited() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        refresh_token_mock("refresh-old")
            .respond_with(
                ResponseTemplate::new(429).set_body_json(json!({ "error": "rate_limited" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_device_authorize(&server, 0).await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let result = auth.access_token().await;

        // Assert
        assert!(matches!(result, Err(AuthError::Http(_))));
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-old"));
    }

    #[tokio::test]
    async fn keeps_refresh_token_on_other_oauth_errors() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        TokenStore::new(dir.path().join("token.json"))
            .save("refresh-old")
            .unwrap();
        refresh_token_mock("refresh-old")
            .respond_with(error_body("invalid_client"))
            .expect(1)
            .mount(&server)
            .await;
        mount_device_authorize(&server, 0).await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let result = auth.access_token().await;

        // Assert
        assert!(matches!(result, Err(AuthError::Rejected(code)) if code == "invalid_client"));
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-old"));
    }

    #[tokio::test]
    async fn runs_device_flow_when_stored_token_file_is_corrupt() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("token.json"), "garbage").unwrap();
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_body("access-1", "refresh-1")),
            )
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let token = auth.access_token().await.unwrap();

        // Assert
        assert_eq!(token, "access-1");
        assert_eq!(stored_token(&dir).as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn returns_access_denied_when_user_rejects_device() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(error_body("access_denied"))
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let result = auth.access_token().await;

        // Assert
        assert!(matches!(result, Err(AuthError::AccessDenied)));
        assert_eq!(stored_token(&dir), None);
    }

    #[tokio::test]
    async fn returns_expired_when_device_code_expires() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        mount_device_authorize(&server, 1).await;
        device_token_mock()
            .respond_with(error_body("expired_token"))
            .mount(&server)
            .await;
        let mut auth = authenticator(&server, &dir);

        // Act
        let result = auth.access_token().await;

        // Assert
        assert!(matches!(result, Err(AuthError::DeviceCodeExpired)));
    }
}
