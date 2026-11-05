//! Per-process credential for dwctl's own calls into its AI proxy.
//!
//! The playground, health probes and the fusillade daemon call `/ai` on the
//! process that serves them: the playground rewrites its request in-process,
//! probes and the daemon loop back to this process's own listen address. They
//! authenticate as an API key without presenting that key's secret, which
//! dwctl stores only as a hash. Instead dwctl mints a short-lived token
//! naming the key:
//!
//! ```text
//! dwi-<api key id, 32 hex>.<expiry, unix seconds>.<HMAC-SHA256, base64url>
//! ```
//!
//! The HMAC key is generated at startup and held only in this process's
//! memory, so a token is accepted only by the process that minted it, and
//! only until it expires or the process restarts. Nothing reusable is
//! persisted: reading a token or the signing key needs access to the running
//! process, which already sees every bearer token it serves.
//!
//! [`internal_credential_middleware`] runs at the front of the `/ai` stack. It
//! verifies a `dwi-` bearer and hands the key on as an
//! [`onwards::auth::PresentedKey`] request extension carrying the key's hash,
//! which onwards and dwctl's own inference layers use in place of hashing the
//! bearer ([`presented_key_hash`]). The key must still be in onwards' live key
//! set, so a deleted key is refused and spend caps still apply. A `dwi-`
//! bearer that fails verification is refused there and then, never passed on
//! as an ordinary key.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{Extensions, StatusCode},
    middleware::Next,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use onwards::{auth::PresentedKey, target::Targets};
use rand::prelude::RngExt;
use sha2::Sha256;
use uuid::Uuid;

use crate::{
    crypto::{ApiKeyHash, hash_api_key},
    inference::rejections,
    types::ApiKeyId,
};

type HmacSha256 = Hmac<Sha256>;

/// Prefix of every internal credential, so it is never mistaken for an API key.
pub const TOKEN_PREFIX: &str = "dwi-";

/// Whether `token` claims to be an internal credential (it may still be invalid).
pub fn is_internal(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX)
}

/// Why an internal credential was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCredential {
    #[error("malformed internal credential")]
    Malformed,
    #[error("internal credential signature does not match")]
    BadSignature,
    #[error("internal credential has expired")]
    Expired,
}

/// The process's signing key: mints and verifies internal credentials.
///
/// Cheap to clone; every clone shares the key. Build one per process with
/// [`InternalCredentials::generate`] and hand the same instance to everything
/// that mints or verifies, or their tokens will not verify.
#[derive(Clone)]
pub struct InternalCredentials {
    key: Arc<[u8; 32]>,
}

impl fmt::Debug for InternalCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InternalCredentials(<redacted>)")
    }
}

impl Default for InternalCredentials {
    fn default() -> Self {
        Self::generate()
    }
}

impl InternalCredentials {
    /// A new random signing key, held only in memory.
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        rand::rng().fill(&mut key);
        Self { key: Arc::new(key) }
    }

    /// A token that authenticates as `api_key_id` until `ttl` from now.
    pub fn mint(&self, api_key_id: ApiKeyId, ttl: Duration) -> String {
        self.sign(api_key_id, unix_now().saturating_add(ttl.as_secs()))
    }

    /// A token that expired, or expires, at `expiry` (unix seconds).
    #[cfg(test)]
    pub(crate) fn mint_expiring_at(&self, api_key_id: ApiKeyId, expiry: u64) -> String {
        self.sign(api_key_id, expiry)
    }

    fn sign(&self, api_key_id: ApiKeyId, expiry: u64) -> String {
        let payload = format!("{TOKEN_PREFIX}{}.{expiry}", api_key_id.simple());
        let tag = self.mac(&payload).finalize().into_bytes();
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(tag))
    }

    /// The key id a token authenticates as, if this process minted it and it
    /// has not expired. The signature is checked, in constant time, before
    /// anything else in the token is read.
    pub fn verify(&self, token: &str) -> Result<ApiKeyId, InvalidCredential> {
        let (id, expiry) = self.verify_signature(token)?;
        if unix_now() >= expiry {
            return Err(InvalidCredential::Expired);
        }
        Ok(id)
    }

    /// The key id and expiry of a token this process minted, whether or not
    /// it has expired.
    fn verify_signature(&self, token: &str) -> Result<(ApiKeyId, u64), InvalidCredential> {
        let (payload, tag) = token.rsplit_once('.').ok_or(InvalidCredential::Malformed)?;
        if !is_internal(payload) {
            return Err(InvalidCredential::Malformed);
        }
        let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| InvalidCredential::Malformed)?;
        self.mac(payload).verify_slice(&tag).map_err(|_| InvalidCredential::BadSignature)?;

        let (id, expiry) = payload[TOKEN_PREFIX.len()..].split_once('.').ok_or(InvalidCredential::Malformed)?;
        let id = Uuid::try_parse(id).map_err(|_| InvalidCredential::Malformed)?;
        let expiry: u64 = expiry.parse().map_err(|_| InvalidCredential::Malformed)?;
        Ok((id, expiry))
    }

    fn mac(&self, payload: &str) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key[..]).expect("a 32-byte key is always a valid HMAC key");
        mac.update(payload.as_bytes());
        mac
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// The hash of the API key an `/ai` request presents: the key
/// [`internal_credential_middleware`] authenticated, else the hash of
/// `bearer`, the token the caller read from the request's headers.
///
/// Every layer inside the `/ai` stack identifies the caller's key with this
/// rather than by hashing the bearer itself, so internal credentials resolve
/// to the key they were minted for.
pub fn presented_key_hash(extensions: &Extensions, bearer: Option<&str>) -> Option<ApiKeyHash> {
    match extensions.get::<PresentedKey>() {
        Some(PresentedKey(hash)) => Some(*hash.as_bytes()),
        None => bearer.map(hash_api_key),
    }
}

/// What [`internal_credential_middleware`] checks tokens against.
#[derive(Clone)]
pub struct InternalAuthState {
    pub credentials: InternalCredentials,
    /// onwards' live key set, for the key id → key hash lookup.
    pub targets: Targets,
}

impl InternalAuthState {
    /// The hash of the key a request captured by request logging presented.
    ///
    /// Request logging sees a request's headers but not its extensions, so
    /// the key [`internal_credential_middleware`] authenticated reaches it on
    /// the response instead ([`carry_presented_key`]): pass that as
    /// `authenticated`. It is set however the request ended, including when
    /// an inner layer refused it before onwards saw it, and does not depend
    /// on the key still being live.
    ///
    /// Without a response (an abandoned request) an internal credential is
    /// resolved again from the header: its signature is checked, its expiry
    /// is not, since request logging sits inside the middleware and so only
    /// ever captures tokens it admitted. Any other bearer is hashed.
    pub fn captured_key_hash(&self, bearer: Option<&str>, authenticated: Option<ApiKeyHash>) -> Option<ApiKeyHash> {
        match bearer {
            Some(token) if is_internal(token) => authenticated.or_else(|| {
                let (id, _expiry) = self.credentials.verify_signature(token).ok()?;
                self.targets.key_ids.get(&id.to_string()).map(|hash| *hash.as_bytes())
            }),
            Some(token) => Some(hash_api_key(token)),
            None => None,
        }
    }

    /// The hash of the live key `token` authenticates as.
    fn resolve(&self, token: &str) -> Result<ApiKeyHash, &'static str> {
        let id = self.credentials.verify(token).map_err(|error| match error {
            InvalidCredential::Malformed => "malformed",
            InvalidCredential::BadSignature => "bad_signature",
            InvalidCredential::Expired => "expired",
        })?;
        self.targets
            .key_ids
            .get(&id.to_string())
            .map(|hash| *hash.as_bytes())
            .ok_or("unknown_key")
    }
}

/// Copies the key a request was authenticated as onto its response, so
/// request logging, which sees a response's extensions but not the
/// request's, can attribute the request to it (see
/// [`InternalAuthState::captured_key_hash`]). Runs just inside request
/// logging. Response extensions are never sent to the client.
pub async fn carry_presented_key(request: Request, next: Next) -> Response {
    let key = request.extensions().get::<PresentedKey>().copied();
    let mut response = next.run(request).await;
    if let Some(key) = key {
        response.extensions_mut().insert(key);
    }
    response
}

/// The key hash [`carry_presented_key`] put on a captured response.
pub fn carried_key_hash(extensions: &Extensions) -> Option<ApiKeyHash> {
    extensions.get::<PresentedKey>().map(|PresentedKey(hash)| *hash.as_bytes())
}

/// Verifies an internal credential at the front of the `/ai` stack and hands
/// the authenticated key on as an [`onwards::auth::PresentedKey`] extension.
/// Requests with any other bearer pass through untouched.
pub async fn internal_credential_middleware(State(state): State<InternalAuthState>, mut request: Request, next: Next) -> Response {
    let Some(token) = rejections::bearer_token(request.headers()).filter(|token| is_internal(token)) else {
        return next.run(request).await;
    };
    match state.resolve(token) {
        Ok(hash) => {
            let hash = onwards::auth::KeyHash::from_bytes(hash);
            request.extensions_mut().insert(PresentedKey(hash));
            next.run(request).await
        }
        Err(reason) => {
            tracing::debug!(reason, "Refused internal credential");
            let response = Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "error": {
                            "message": "Invalid or expired internal credential",
                            "type": "invalid_request_error",
                            "code": "invalid_api_key",
                        }
                    })
                    .to_string(),
                ))
                .expect("static response always builds");
            rejections::refuse(&request, response, "invalid_api_key", None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{Router, routing::post};
    use axum_test::TestServer;

    use super::*;

    #[test]
    fn mint_then_verify_round_trips() {
        let credentials = InternalCredentials::generate();
        let id = Uuid::new_v4();
        let token = credentials.mint(id, Duration::from_secs(60));
        assert!(is_internal(&token));
        assert!(!token.contains(&id.to_string()), "id is written without hyphens");
        assert_eq!(credentials.verify(&token), Ok(id));
    }

    #[test]
    fn rejects_expired_tokens() {
        let credentials = InternalCredentials::generate();
        let token = credentials.mint_expiring_at(Uuid::new_v4(), unix_now() - 1);
        assert_eq!(credentials.verify(&token), Err(InvalidCredential::Expired));
    }

    #[test]
    fn rejects_tokens_signed_by_another_process() {
        let ours = InternalCredentials::generate();
        let theirs = InternalCredentials::generate();
        let token = theirs.mint(Uuid::new_v4(), Duration::from_secs(60));
        assert_eq!(ours.verify(&token), Err(InvalidCredential::BadSignature));
    }

    #[test]
    fn rejects_forged_and_malformed_tokens() {
        let credentials = InternalCredentials::generate();
        let token = credentials.mint(Uuid::new_v4(), Duration::from_secs(60));
        let (payload, tag) = token.rsplit_once('.').unwrap();

        // Another key id, or a later expiry, under the original signature.
        let other_id = format!("{TOKEN_PREFIX}{}.{}", Uuid::new_v4().simple(), unix_now() + 60);
        let later = format!("{}.{}", payload.rsplit_once('.').unwrap().0, unix_now() + 3600);
        for forged in [other_id, later] {
            assert_eq!(
                credentials.verify(&format!("{forged}.{tag}")),
                Err(InvalidCredential::BadSignature),
                "{forged}"
            );
        }

        for malformed in ["", "dwi-", "sk-abc", "dwi-abc.def", &format!("{payload}.not base64!")] {
            assert_eq!(credentials.verify(malformed), Err(InvalidCredential::Malformed), "{malformed}");
        }
        // A valid signature over a payload that isn't `<id>.<expiry>`.
        let unparseable = credentials
            .mint_expiring_at(Uuid::nil(), 0)
            .replace(&Uuid::nil().simple().to_string(), "x");
        assert!(credentials.verify(&unparseable).is_err());
    }

    /// Request logging resolves a captured token by signature alone (it was
    /// checked in full on the way in), never trusting a foreign one.
    #[test]
    fn captured_key_hash_checks_the_signature_not_the_expiry() {
        let state = InternalAuthState {
            credentials: InternalCredentials::generate(),
            targets: Targets::default(),
        };
        let id = Uuid::new_v4();
        state.targets.key_ids.insert(id.to_string(), onwards::auth::KeyHash::of("sk-live"));
        let live = Some(hash_api_key("sk-live"));

        assert_eq!(
            state.captured_key_hash(Some(&state.credentials.mint(id, Duration::from_secs(60))), None),
            live
        );
        assert_eq!(
            state.captured_key_hash(Some(&state.credentials.mint_expiring_at(id, 1)), None),
            live
        );
        let foreign = InternalCredentials::generate().mint(id, Duration::from_secs(60));
        assert_eq!(state.captured_key_hash(Some(&foreign), None), None);
        assert_eq!(state.captured_key_hash(Some("sk-other"), None), Some(hash_api_key("sk-other")));
        assert_eq!(state.captured_key_hash(None, None), None);

        // The key carried on the response wins, and needs no live key: a key
        // deleted while its request was in flight is still attributed.
        let deleted = Uuid::new_v4();
        let carried = Some(hash_api_key("sk-deleted"));
        let token = state.credentials.mint(deleted, Duration::from_secs(60));
        assert_eq!(state.captured_key_hash(Some(&token), None), None);
        assert_eq!(state.captured_key_hash(Some(&token), carried), carried);
        // Only for internal credentials: a plain bearer is its own key.
        assert_eq!(state.captured_key_hash(Some("sk-other"), carried), Some(hash_api_key("sk-other")));
    }

    /// The authenticated key reaches request logging on the response, also
    /// when an inner layer refuses the request.
    #[tokio::test]
    async fn carry_presented_key_puts_the_key_on_the_response() {
        use tower::ServiceExt;

        async fn refuse() -> StatusCode {
            StatusCode::BAD_REQUEST
        }
        let app = Router::new()
            .route("/ai", post(refuse))
            .layer(axum::middleware::from_fn(carry_presented_key));
        let key = onwards::auth::KeyHash::of("sk-live");
        let mut request = Request::builder().method("POST").uri("/ai").body(Body::empty()).unwrap();
        request.extensions_mut().insert(PresentedKey(key));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(carried_key_hash(response.extensions()), Some(hash_api_key("sk-live")));

        let plain = Request::builder().method("POST").uri("/ai").body(Body::empty()).unwrap();
        assert_eq!(carried_key_hash(app.oneshot(plain).await.unwrap().extensions()), None);
    }

    #[test]
    fn debug_hides_the_key() {
        assert_eq!(format!("{:?}", InternalCredentials::generate()), "InternalCredentials(<redacted>)");
    }

    #[test]
    fn presented_key_hash_prefers_the_authenticated_key() {
        let mut extensions = Extensions::new();
        assert_eq!(presented_key_hash(&extensions, None), None);
        assert_eq!(presented_key_hash(&extensions, Some("sk-a")), Some(hash_api_key("sk-a")));
        extensions.insert(PresentedKey(onwards::auth::KeyHash::of("sk-b")));
        assert_eq!(presented_key_hash(&extensions, Some("sk-a")), Some(hash_api_key("sk-b")));
    }

    /// The middleware admits a valid token for a live key as that key, and
    /// refuses forged, expired, foreign and deleted-key tokens without
    /// reaching the inner service.
    #[tokio::test]
    async fn middleware_admits_only_valid_tokens_for_live_keys() {
        let credentials = InternalCredentials::generate();
        let live = Uuid::new_v4();
        let deleted = Uuid::new_v4();
        let targets = Targets::default();
        targets.key_ids.insert(live.to_string(), onwards::auth::KeyHash::of("sk-live"));

        async fn echo(request: Request) -> String {
            match request.extensions().get::<PresentedKey>() {
                Some(PresentedKey(hash)) if *hash == onwards::auth::KeyHash::of("sk-live") => "live".to_string(),
                Some(_) => "other".to_string(),
                None => "none".to_string(),
            }
        }
        let app = Router::new().route("/ai", post(echo)).layer(axum::middleware::from_fn_with_state(
            InternalAuthState {
                credentials: credentials.clone(),
                targets,
            },
            internal_credential_middleware,
        ));
        let server = TestServer::new(app).unwrap();

        let ok = server
            .post("/ai")
            .authorization_bearer(credentials.mint(live, Duration::from_secs(60)))
            .await;
        ok.assert_status_ok();
        assert_eq!(ok.text(), "live");

        // An ordinary key is not this layer's business.
        let plain = server.post("/ai").authorization_bearer("sk-live").await;
        assert_eq!(plain.text(), "none");

        let refused: HashMap<&str, String> = HashMap::from([
            ("deleted key", credentials.mint(deleted, Duration::from_secs(60))),
            ("expired", credentials.mint_expiring_at(live, unix_now() - 1)),
            ("other process", InternalCredentials::generate().mint(live, Duration::from_secs(60))),
            ("garbage", format!("{TOKEN_PREFIX}garbage")),
        ]);
        for (case, token) in refused {
            let response = server.post("/ai").authorization_bearer(token).await;
            assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED, "{case}");
            assert_eq!(response.json::<serde_json::Value>()["error"]["code"], "invalid_api_key", "{case}");
        }
    }
}
