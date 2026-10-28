// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Sign-in through the central identity provider: OpenID Connect code flow with
//! PKCE, as a confidential client. The provider holds e-mail and password; this
//! module validates its tokens and hands the account to the local session.
mod routes;
#[cfg(test)]
mod tests;

pub use routes::routes;

use chrono::{DateTime, Utc};
use std::{fmt, sync::Mutex, time::Duration};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use openidconnect::{
    AdditionalClaims, AdditionalProviderMetadata, AuthorizationCode, Client, ClientId,
    ClientSecret, CsrfToken, EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IdTokenFields, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    StandardErrorResponse, StandardTokenResponse, TokenResponse as _,
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreClaimName, CoreClaimType,
        CoreClientAuthMethod, CoreErrorResponseType, CoreGenderClaim, CoreGrantType,
        CoreJsonWebKey, CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm,
        CoreJwsSigningAlgorithm, CoreResponseMode, CoreResponseType, CoreRevocableToken,
        CoreRevocationErrorResponse, CoreSubjectIdentifierType, CoreTokenIntrospectionResponse,
        CoreTokenType,
    },
    reqwest,
    url::Url,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);
/// A key rotation at the provider is picked up at most this often.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// While the provider has never answered, a sign-in may retry this often.
const MIN_DISCOVERY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_DISCOVERY_BACKOFF: Duration = Duration::from_secs(60);
const BACKCHANNEL_LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// Claims the realm adds for an installation client: its roles and the
/// provider session, which back-channel logout names.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EstacionaClaims {
    #[serde(default)]
    estaciona_roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sid: Option<String>,
}
impl AdditionalClaims for EstacionaClaims {}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LogoutMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end_session_endpoint: Option<Url>,
}
impl AdditionalProviderMetadata for LogoutMetadata {}

type ProviderMetadata = openidconnect::ProviderMetadata<
    LogoutMetadata,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;
type TokenResponse = StandardTokenResponse<
    IdTokenFields<
        EstacionaClaims,
        EmptyExtraTokenFields,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    CoreTokenType,
>;
type OidcClient = Client<
    EstacionaClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// HTTPS, or plain HTTP to this machine for development; no query or fragment.
fn secure_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let secure = url.scheme() == "https" || (url.scheme() == "http" && local);
    (secure
        && url.host_str().is_some()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// Never derive Debug: it holds the client secret.
pub struct IdentityConfig {
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: ClientSecret,
    redirect: RedirectUrl,
    app_url: String,
}

impl IdentityConfig {
    /// Errors name the offending setting only.
    pub fn new(
        issuer: &str,
        client_id: &str,
        client_secret: String,
        redirect: &str,
        app_url: &str,
    ) -> Result<Self, &'static str> {
        secure_url(issuer).ok_or("OIDC_ISSUER_URL")?;
        let issuer = IssuerUrl::new(issuer.to_owned()).map_err(|_| "OIDC_ISSUER_URL")?;
        if client_id.is_empty() || client_id.len() > 255 {
            return Err("OIDC_CLIENT_ID");
        }
        let redirect = secure_url(redirect)
            .and_then(|url| RedirectUrl::new(url.to_string()).ok())
            .ok_or("OIDC_REDIRECT_URL")?;
        let app_url = secure_url(app_url).ok_or("APP_URL")?;
        Ok(Self {
            issuer,
            client_id: ClientId::new(client_id.to_owned()),
            client_secret: ClientSecret::new(client_secret),
            redirect,
            app_url: app_url.as_str().trim_end_matches('/').to_owned(),
        })
    }

    pub fn app_url(&self) -> &str {
        &self.app_url
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum IdentityError {
    Discovery,
    Exchange,
    InvalidToken,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Discovery => "Identity provider metadata unavailable",
            Self::Exchange => "Identity provider rejected or failed the code exchange",
            Self::InvalidToken => "Identity provider token failed validation",
        })
    }
}

impl std::error::Error for IdentityError {}

/// What the browser carries to the provider, and what stays here to finish.
pub struct SignInStart {
    pub authorize_url: Url,
    pub state: String,
    pub pkce_verifier: String,
    pub nonce: String,
}

/// A validated ID token, reduced to what the installation keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub issuer: String,
    pub subject: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub roles: Vec<String>,
    pub provider_session: Option<String>,
    /// When the provider issued the ID token, on the provider's clock.
    pub issued_at: DateTime<Utc>,
}

/// Sessions a validated back-channel logout token ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutTarget {
    ProviderSession(String),
    Subject(String),
}

/// A validated back-channel logout: what it ends, and since when, on the same
/// provider clock that stamps ID tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logout {
    pub target: LogoutTarget,
    pub issued_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct LogoutClaims {
    jti: String,
    iat: i64,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    sid: Option<String>,
    events: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
}

struct Discovered {
    client: OidcClient,
    keys: JwkSet,
    end_session: Option<Url>,
}

/// The installation keeps serving existing sessions while the central provider
/// is unreachable; only sign-in, logout redirection and back-channel logout
/// wait for its metadata.
pub struct IdentityProvider {
    config: IdentityConfig,
    http: reqwest::Client,
    discovered: RwLock<Option<Discovered>>,
    last_refresh: Mutex<Option<std::time::Instant>>,
}

impl IdentityProvider {
    pub fn new(config: IdentityConfig) -> Result<Self, IdentityError> {
        let http = reqwest::ClientBuilder::new()
            // Following redirects would let metadata point requests elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|_| IdentityError::Discovery)?;
        Ok(Self {
            config,
            http,
            discovered: RwLock::new(None),
            last_refresh: Mutex::new(None),
        })
    }

    /// For callers that need the provider now, such as tests.
    #[cfg(test)]
    pub async fn discover(config: IdentityConfig) -> Result<Self, IdentityError> {
        let provider = Self::new(config)?;
        *provider.discovered.write().await = Some(fetch(&provider.config, &provider.http).await?);
        Ok(provider)
    }

    /// Runs in the background from startup until the provider first answers.
    pub async fn discover_until_ready(&self) {
        let mut delay = Duration::from_millis(500);
        while self.ready().await.is_err() {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_DISCOVERY_BACKOFF);
        }
        tracing::info!("Identity provider metadata loaded");
    }

    /// Loads metadata on first use; a provider outage costs one attempt per interval.
    async fn ready(&self) -> Result<(), IdentityError> {
        if self.discovered.read().await.is_some() {
            return Ok(());
        }
        if !self.claim_attempt(MIN_DISCOVERY_INTERVAL) {
            return Err(IdentityError::Discovery);
        }
        let fresh = fetch(&self.config, &self.http).await?;
        self.discovered.write().await.get_or_insert(fresh);
        Ok(())
    }

    fn claim_attempt(&self, interval: Duration) -> bool {
        let Ok(mut last) = self.last_refresh.lock() else {
            return false;
        };
        if last.is_some_and(|last| last.elapsed() < interval) {
            return false;
        }
        *last = Some(std::time::Instant::now());
        true
    }

    pub fn config(&self) -> &IdentityConfig {
        &self.config
    }

    /// No network: synthetic metadata and an empty key set, so any token fails.
    #[cfg(test)]
    pub fn for_unit_test() -> Self {
        let config = IdentityConfig::new(
            "https://auth.example.invalid/realms/estaciona",
            "unit-test-installation",
            "unit-test-client-secret".to_owned(),
            "https://api.example.invalid/auth/callback",
            "https://app.example.invalid",
        )
        .expect("unit test identity configuration");
        let metadata: ProviderMetadata = serde_json::from_value(serde_json::json!({
            "issuer": "https://auth.example.invalid/realms/estaciona",
            "authorization_endpoint": "https://auth.example.invalid/realms/estaciona/protocol/openid-connect/auth",
            "token_endpoint": "https://auth.example.invalid/realms/estaciona/protocol/openid-connect/token",
            "jwks_uri": "https://auth.example.invalid/realms/estaciona/protocol/openid-connect/certs",
            "end_session_endpoint": "https://auth.example.invalid/realms/estaciona/protocol/openid-connect/logout",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"]
        }))
        .expect("unit test provider metadata");
        let end_session = metadata.additional_metadata().end_session_endpoint.clone();
        let client = OidcClient::from_provider_metadata(
            metadata,
            config.client_id.clone(),
            Some(config.client_secret.clone()),
        )
        .set_redirect_uri(config.redirect.clone());
        Self {
            config,
            http: reqwest::Client::new(),
            discovered: RwLock::new(Some(Discovered {
                client,
                keys: JwkSet { keys: Vec::new() },
                end_session,
            })),
            last_refresh: Mutex::new(Some(std::time::Instant::now())),
        }
    }

    pub async fn start(&self) -> Result<SignInStart, IdentityError> {
        self.ready().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let guard = self.discovered.read().await;
        let discovered = guard.as_ref().ok_or(IdentityError::Discovery)?;
        let (authorize_url, state, nonce) = discovered
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                || CsrfToken::new_random_len(32),
                || Nonce::new_random_len(32),
            )
            .add_scope(Scope::new("profile".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(SignInStart {
            authorize_url,
            state: state.secret().to_owned(),
            pkce_verifier: verifier.secret().to_owned(),
            nonce: nonce.secret().to_owned(),
        })
    }

    pub async fn finish(
        &self,
        code: String,
        pkce_verifier: String,
        nonce: String,
    ) -> Result<SignedIn, IdentityError> {
        self.ready().await?;
        let response = {
            let guard = self.discovered.read().await;
            let discovered = guard.as_ref().ok_or(IdentityError::Discovery)?;
            discovered
                .client
                .exchange_code(AuthorizationCode::new(code))
                .map_err(|_| IdentityError::Exchange)?
                .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
                .request_async(&self.http)
                .await
                .map_err(|error| {
                    tracing::warn!(error = %error, "Code exchange failed");
                    IdentityError::Exchange
                })?
        };
        let id_token = response.id_token().ok_or(IdentityError::InvalidToken)?;
        let nonce = Nonce::new(nonce);
        let verified = self.verify_id_token(id_token, &nonce).await;
        let claims = match verified {
            Ok(claims) => claims,
            // An unknown signing key may be a rotation: refresh once and retry.
            Err(_) if self.refresh().await => self.verify_id_token(id_token, &nonce).await?,
            Err(error) => return Err(error),
        };
        Ok(claims)
    }

    async fn verify_id_token(
        &self,
        id_token: &openidconnect::IdToken<
            EstacionaClaims,
            CoreGenderClaim,
            CoreJweContentEncryptionAlgorithm,
            CoreJwsSigningAlgorithm,
        >,
        nonce: &Nonce,
    ) -> Result<SignedIn, IdentityError> {
        let guard = self.discovered.read().await;
        let discovered = guard.as_ref().ok_or(IdentityError::Discovery)?;
        let verifier = discovered
            .client
            .id_token_verifier()
            .set_allowed_algs([CoreJwsSigningAlgorithm::EdDsa]);
        let claims = id_token.claims(&verifier, nonce).map_err(|error| {
            tracing::warn!(error = %error, "ID token rejected");
            IdentityError::InvalidToken
        })?;
        let display_name = claims
            .name()
            .and_then(|name| name.get(None))
            .map(|name| name.as_str().to_owned())
            .or_else(|| {
                let given = claims.given_name().and_then(|name| name.get(None));
                let family = claims.family_name().and_then(|name| name.get(None));
                let parts: Vec<&str> = [
                    given.map(|name| name.as_str()),
                    family.map(|name| name.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect();
                (!parts.is_empty()).then(|| parts.join(" "))
            });
        Ok(SignedIn {
            issuer: claims.issuer().as_str().to_owned(),
            subject: claims.subject().as_str().to_owned(),
            username: claims
                .preferred_username()
                .map(|username| username.as_str().to_owned()),
            display_name,
            roles: claims.additional_claims().estaciona_roles.clone(),
            provider_session: claims.additional_claims().sid.clone(),
            issued_at: claims.issue_time(),
        })
    }

    /// Validates an OIDC Back-Channel Logout 1.0 token.
    pub async fn verify_logout(&self, token: &str) -> Result<Logout, IdentityError> {
        self.ready().await?;
        match self.decode_logout(token).await {
            Err(_) if self.refresh().await => self.decode_logout(token).await,
            result => result,
        }
    }

    async fn decode_logout(&self, token: &str) -> Result<Logout, IdentityError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| IdentityError::InvalidToken)?;
        if header.alg != Algorithm::EdDSA {
            return Err(IdentityError::InvalidToken);
        }
        let kid = header.kid.ok_or(IdentityError::InvalidToken)?;
        let key = {
            let guard = self.discovered.read().await;
            let discovered = guard.as_ref().ok_or(IdentityError::Discovery)?;
            let jwk = discovered
                .keys
                .find(&kid)
                .ok_or(IdentityError::InvalidToken)?;
            DecodingKey::from_jwk(jwk).map_err(|_| IdentityError::InvalidToken)?
        };
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[self.config.client_id.as_str()]);
        validation.set_required_spec_claims(&["iss", "aud", "iat", "exp"]);
        validation.leeway = 30;
        let claims = jsonwebtoken::decode::<LogoutClaims>(token, &key, &validation)
            .map_err(|_| IdentityError::InvalidToken)?
            .claims;
        if claims.jti.is_empty()
            || claims.nonce.is_some()
            || !claims.events.contains_key(BACKCHANNEL_LOGOUT_EVENT)
        {
            return Err(IdentityError::InvalidToken);
        }
        let issued_at =
            DateTime::from_timestamp(claims.iat, 0).ok_or(IdentityError::InvalidToken)?;
        let target = match (claims.sid, claims.sub) {
            (Some(sid), _) if !sid.is_empty() => LogoutTarget::ProviderSession(sid),
            (_, Some(sub)) if !sub.is_empty() => LogoutTarget::Subject(sub),
            _ => return Err(IdentityError::InvalidToken),
        };
        Ok(Logout { target, issued_at })
    }

    /// Re-reads metadata and keys, at most once per interval. Returns whether
    /// the keys changed, so a failed validation is worth retrying.
    async fn refresh(&self) -> bool {
        if !self.claim_attempt(MIN_REFRESH_INTERVAL) {
            return false;
        }
        match fetch(&self.config, &self.http).await {
            Ok(fresh) => {
                let mut discovered = self.discovered.write().await;
                let changed = discovered.as_ref().is_none_or(|current| {
                    serde_json::to_value(&fresh.keys).ok()
                        != serde_json::to_value(&current.keys).ok()
                });
                *discovered = Some(fresh);
                changed
            }
            Err(error) => {
                tracing::warn!(%error, "Identity provider refresh failed");
                false
            }
        }
    }

    /// Ends the provider session too; without an ID token hint the provider
    /// asks the person to confirm.
    pub async fn end_session_url(&self) -> Option<String> {
        self.ready().await.ok()?;
        let guard = self.discovered.read().await;
        let mut url = guard.as_ref()?.end_session.clone()?;
        url.query_pairs_mut()
            .append_pair("client_id", self.config.client_id.as_str())
            .append_pair(
                "post_logout_redirect_uri",
                &format!("{}/", self.config.app_url),
            );
        Some(url.into())
    }
}

async fn fetch(
    config: &IdentityConfig,
    http: &reqwest::Client,
) -> Result<Discovered, IdentityError> {
    let metadata = ProviderMetadata::discover_async(config.issuer.clone(), http)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "Identity provider discovery failed");
            IdentityError::Discovery
        })?;
    let keys: JwkSet = serde_json::to_value(metadata.jwks())
        .and_then(serde_json::from_value)
        .map_err(|_| IdentityError::Discovery)?;
    let end_session = metadata.additional_metadata().end_session_endpoint.clone();
    let client = OidcClient::from_provider_metadata(
        metadata,
        config.client_id.clone(),
        Some(config.client_secret.clone()),
    )
    .set_redirect_uri(config.redirect.clone());
    Ok(Discovered {
        client,
        keys,
        end_session,
    })
}
