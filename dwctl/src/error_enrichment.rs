//! Error enrichment middleware for AI proxy requests.
//!
//! This module provides middleware that intercepts error responses from the onwards
//! routing layer and enriches them with helpful context. The primary use case is
//! transforming generic 403 errors (insufficient credits) into detailed responses
//! that explain the issue and suggest remediation.
//!
//! ## Architecture
//!
//! The middleware sits in the response path after onwards but before outlet:
//! ```text
//! Request → onwards (routing) → error_enrichment (this) → outlet (logging) → Response
//! ```
//!
//! ## Error Cases Enriched
//!
//! 0. **403 Forbidden - Non-inference key**: A `platform` (management) key was
//!    used for inference. Onwards excludes these from its key set, so they 403
//!    with a generic body; we explain the real reason.
//! 1. **403 Forbidden - Insufficient Credits**: User's balance < 0 for paid models
//!    - Shows current balance
//! 2. **403 Forbidden - Model Access Denied**: User is not a member of a group with access to the requested model
//!    - Shows which model was requested
//! 3. **403 Forbidden - Modality Blocked**: A traffic routing rule denies the API key's
//!    purpose (realtime/batch/playground) for the requested model
//!    - Shows which modality and model are blocked

use crate::{
    db::errors::DbError,
    db::handlers::{Credits, api_keys::ApiKeys},
    errors::Error,
    types::UserId,
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Request body structure for extracting model name
#[derive(Debug, Deserialize)]
struct ChatRequest {
    model: String,
}

/// Middleware that enriches error responses from the AI proxy with helpful context
///
/// Currently handles:
/// - 403 Forbidden errors (likely insufficient credits) → enriched with balance
/// - 403 Forbidden errors (likely model access denied) → enriched with model name
/// - 403 Forbidden errors (likely modality blocked by routing rule) → enriched with modality + model
/// - 403 Forbidden errors (spending cap exhausted) → rewritten to 402 with cap details
/// - 403 Forbidden errors (cap window rolled, reinstatement pending) → retriable 429
///   plus a demand-driven config resync so the retry succeeds within seconds
#[instrument(name = "dwctl.error_enrichment", skip_all, fields(http.request.method = %request.method(), url.path = %request.uri().path(), url.query = request.uri().query().unwrap_or("")))]
pub async fn error_enrichment_middleware(State(pool): State<PgPool>, request: Request<Body>, next: Next) -> Response<Body> {
    // Extract API key from request headers before passing to onwards
    let api_key = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")))
        .map(|token| token.trim().to_string());

    // Extract request body to get model name for potential error enrichment
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            // If we can't read the body, just pass through
            let reconstructed = Request::from_parts(parts, Body::empty());
            return next.run(reconstructed).await;
        }
    };

    let model_name = serde_json::from_slice::<ChatRequest>(&bytes).ok().map(|req| req.model);

    // Reconstruct the request with the body
    let reconstructed = Request::from_parts(parts, Body::from(bytes));

    // Let the request proceed through onwards
    let response = next.run(reconstructed).await;

    // Only enrich 403 errors when we have an API key
    // Note: This middleware is applied only to the onwards router (AI proxy paths),
    // so no path filtering is needed here
    if response.status() == StatusCode::FORBIDDEN
        && let Some(key) = api_key
    {
        debug!("Intercepted 403 response on AI proxy path, attempting enrichment");

        // One lookup of the key's owner + purpose, reused by the checks below so
        // a 403 does not fan out into several by-secret queries. None for an
        // unknown/invalid token, in which case the checks fall through.
        let key_info = get_api_key_user_and_purpose(pool.clone(), &key).await.ok().flatten();

        // Order matters: the more fundamental the failure, the earlier it runs, so
        // when several conditions could explain the 403 we surface the one that's
        // most useful to act on.
        //   0. Non-inference purpose (e.g. platform) — onwards excludes these from
        //      its key set, so the request can't do inference at all.
        //   1. Model access (group membership) — without this the user can't reach
        //      the model at all, so report it first.
        //   2. Modality (traffic routing rule) — user has the model but their key
        //      kind (batch/realtime/playground) is denied.
        //   3. Insufficient balance — onwards excludes keys with balance ≤ 0.
        //      Balance deliberately supersedes the spending cap below: if both
        //      are blown, the account-level condition is the fundamental,
        //      actionable one.
        //   4. Spending cap — onwards excludes every key of a cap scope whose
        //      window spend reached the limit; only reported when balance is
        //      healthy.

        // 0. Non-inference key: explain why an otherwise-valid key was rejected,
        //    rather than leaving onwards' generic "forbidden" body.
        if let Some((_, purpose)) = key_info.as_ref()
            && !crate::db::models::api_keys::is_inference_purpose(purpose)
        {
            let body = serde_json::json!({
                "error": {
                    "message": format!("API keys with purpose '{purpose}' cannot be used for inference requests."),
                    "type": "invalid_request_error"
                }
            });
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response());
        }

        // 1. Model access via group membership
        if let Some(model) = &model_name
            && let Some((user_id, _)) = key_info.as_ref()
            && let Ok(has_access) = check_user_has_model_access(pool.clone(), *user_id, model).await
            && !has_access
        {
            return Error::ModelAccessDenied {
                model_name: model.clone(),
                message: format!(
                    "You do not have access to '{}'. Please contact your administrator to request access.",
                    model
                ),
            }
            .into_response();
        }

        // 2. Modality blocked by a traffic routing rule on this model.
        if let Some(model) = &model_name
            && let Ok(Some(purpose)) = check_modality_blocked(pool.clone(), &key, model).await
        {
            return Error::ModalityAccessDenied {
                model_name: model.clone(),
                purpose: purpose.clone(),
                message: modality_blocked_message(&purpose, model),
            }
            .into_response();
        }

        // 3. Insufficient balance.
        if let Ok(balance) = get_balance_of_api_key(pool.clone(), &key).await
            && balance <= Decimal::ZERO
        {
            return Error::InsufficientCredits {
                current_balance: balance,
                message: "Account balance too low. Please add credits to continue.".to_string(),
            }
            .into_response();
        }

        // 4. Spending cap. Read-only against the same checkpoint state and
        //    window function the sync eligibility predicate uses. The
        //    window-currency check keeps this arm honest during the small
        //    post-boundary lag: a key whose window has rolled but which the
        //    periodic fallback sync hasn't readmitted yet is never reported
        //    as "cap exceeded".
        if let Ok(Some(cap)) = get_spend_cap_state(pool.clone(), &key).await
            && cap.window_spend >= cap.limit
        {
            if cap.window_current {
                let resets = match cap.resets_at {
                    Some(at) => format!("; resets {}", at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                    None => String::new(),
                };
                let body = serde_json::json!({
                    "error": {
                        "message": format!(
                            "API key has reached its spending cap of ${} (spent ${} this period{resets}). Raise or remove the cap to resume.",
                            cap.limit.round_dp(2),
                            cap.window_spend.round_dp(2),
                        ),
                        "type": "insufficient_quota",
                        "code": "spend_cap_exceeded",
                        "param": null
                    }
                });
                return Response::builder()
                    .status(StatusCode::PAYMENT_REQUIRED)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap_or_else(|_| StatusCode::PAYMENT_REQUIRED.into_response());
            }

            // The window has ROLLED but the sync hasn't readmitted the scope
            // yet. Fire a demand-driven resync so the NEXT request — and
            // every other drained capped key whose window rolled at the same
            // boundary — finds its key back in onwards within ~seconds
            // (throttled per pod so a degraded/slow reload can't be amplified
            // into a resync storm by a burst of 403s; if the notify is
            // throttled or lost, the periodic fallback sync readmits within
            // one interval anyway).
            maybe_fire_boundary_resync(&pool).await;

            // And tell the triggering request the truth: its cap has reset
            // and the key is being reinstated. 429 deliberately, because it
            // is retriable everywhere it matters — fusillade's daemon retries
            // 429s (so a batch spanning the boundary loses nothing, the
            // request just lands after readmission) and client SDKs back off
            // and retry automatically. The short Retry-After reflects the
            // notify-driven readmission (~seconds), not the fallback backstop.
            // Deliberately not a 5xx — that would page the proxy-errors
            // alert for a benign, self-healing race.
            let body = serde_json::json!({
                "error": {
                    "message": "Spending cap window has reset; the key is being reinstated. Retry shortly.",
                    "type": "rate_limit_error",
                    "code": "spend_cap_reset_pending",
                    "param": null
                }
            });
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("content-type", "application/json")
                .header("retry-after", "5")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response());
        }
    }

    response
}

/// Per-pod throttle for the demand-driven boundary resync: at most one NOTIFY
/// per minute. One fire readmits every rolled scope at once (full config
/// reload), so the throttle only bites in the pathological case — reloads slow
/// or failing while capped traffic keeps 403ing — which is exactly when extra
/// reload pressure must be avoided.
static LAST_BOUNDARY_RESYNC_EPOCH_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const BOUNDARY_RESYNC_MIN_INTERVAL_SECS: u64 = 60;

#[cfg(test)]
pub(crate) fn reset_boundary_resync_throttle_for_tests() {
    LAST_BOUNDARY_RESYNC_EPOCH_SECS.store(0, std::sync::atomic::Ordering::Relaxed);
}

async fn maybe_fire_boundary_resync(pool: &PgPool) {
    use std::sync::atomic::Ordering;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = LAST_BOUNDARY_RESYNC_EPOCH_SECS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < BOUNDARY_RESYNC_MIN_INTERVAL_SECS {
        return;
    }
    // Single winner per throttle window per pod; losers of the race skip.
    if LAST_BOUNDARY_RESYNC_EPOCH_SECS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let epoch_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let payload = format!("api_key_spend_cap_boundary:{epoch_micros}");
    // Best-effort: on failure the fallback sync readmits within the interval.
    if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL)
        .bind(&payload)
        .execute(pool)
        .await
    {
        debug!(error = %e, "boundary resync notify failed (best-effort; fallback sync will readmit)");
    }
}

/// Spending-cap state for the cap scope of the key with this secret (the key
/// itself, or its capped parent when the secret belongs to a cap-scope child).
/// `None` when the key is unknown or its scope has no cap set.
struct SpendCapState {
    limit: Decimal,
    window_spend: Decimal,
    /// Whether `window_started_at` falls in the current calendar window (UTC).
    /// False for an exhausted-but-rolled window = reinstatement pending.
    window_current: bool,
    /// Next calendar boundary for windowed caps; `None` for one-off caps.
    resets_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[instrument(skip_all, name = "dwctl.get_spend_cap_state")]
async fn get_spend_cap_state(pool: PgPool, api_key: &str) -> Result<Option<SpendCapState>, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT root.spend_limit AS "spend_limit!",
               COALESCE(ck.window_spend, 0) AS "window_spend!",
               api_key_cap_window_current(ck.window_started_at, root.spend_limit_interval) AS "window_current!",
               api_key_cap_window_resets_at(root.spend_limit_interval) AS resets_at
        FROM api_keys ak
        JOIN api_keys root ON root.id = COALESCE(ak.parent_api_key_id, ak.id)
        LEFT JOIN api_key_spend_checkpoints ck ON ck.api_key_id = root.id
        WHERE ak.secret = $1 AND ak.is_deleted = false AND root.spend_limit IS NOT NULL
        "#,
        api_key
    )
    .fetch_optional(&pool)
    .await?;

    Ok(row.map(|r| SpendCapState {
        limit: r.spend_limit,
        window_spend: r.window_spend,
        window_current: r.window_current,
        resets_at: r.resets_at,
    }))
}

/// Render an `api_keys.purpose` value as a user-facing modality label.
///
/// Returns owned `String` so unknown purposes can fall back to a capitalised
/// form of the raw value (rather than a generic placeholder), keeping the
/// message accurate if new purposes are added without updating this match.
fn modality_label(purpose: &str) -> String {
    match purpose {
        "batch" => "Batch".to_string(),
        "realtime" => "Real-time".to_string(),
        "playground" => "Playground".to_string(),
        "platform" => "Platform".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "This".to_string(),
            }
        }
    }
}

/// The modality a Flex request is dispatched as.
///
/// Flex bodies are queued and later dispatched through `onwards` using the key
/// owner's hidden **batch** key (see `resolve_flex_batch_api_key`), so the
/// modality a Flex request must be checked against is `batch` — NOT the caller's
/// own key purpose. A `realtime` deny rule is intended to push users onto the
/// async/Flex path, so it must not block Flex; only a `batch` deny rule (which
/// `onwards` would itself enforce once the daemon dispatches the request) should.
const FLEX_DISPATCH_PURPOSE: &str = "batch";

/// Check whether a traffic routing rule denies this API key's purpose for the given model.
///
/// Returns `Ok(Some(purpose))` if a matching `deny` rule exists, where `purpose` is the
/// `api_key_purpose` string stored on the rule (e.g. "batch"). Returns `Ok(None)` otherwise.
#[instrument(skip(pool, api_key), name = "dwctl.check_modality_blocked")]
pub async fn check_modality_blocked(pool: PgPool, api_key: &str, model_alias: &str) -> Result<Option<String>, DbError> {
    let mut conn = pool.acquire().await?;

    let purpose = sqlx::query_scalar!(
        r#"
        SELECT mtr.api_key_purpose
        FROM model_traffic_rules mtr
        JOIN deployed_models dm ON dm.id = mtr.deployed_model_id
        JOIN api_keys ak ON ak.purpose = mtr.api_key_purpose
        WHERE dm.alias = $1
          AND dm.deleted = false
          AND ak.secret = $2
          AND ak.is_deleted = false
          AND mtr.action = 'deny'
        LIMIT 1
        "#,
        model_alias,
        api_key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(purpose)
}

/// Check whether a `deny` traffic routing rule exists for a specific `purpose` on
/// the given model, independent of any API key.
///
/// Returns `Ok(Some(purpose))` if a matching rule exists, `Ok(None)` otherwise.
/// Unlike [`check_modality_blocked`], the purpose to enforce is supplied by the
/// caller rather than derived from the presented key's own `purpose`. The Flex
/// path uses this with [`FLEX_DISPATCH_PURPOSE`], because a Flex request executes
/// as `batch` regardless of the purpose of the key that submitted it.
#[instrument(skip(pool), name = "dwctl.check_modality_blocked_for_purpose")]
pub async fn check_modality_blocked_for_purpose(pool: PgPool, model_alias: &str, purpose: &str) -> Result<Option<String>, DbError> {
    let mut conn = pool.acquire().await?;

    let found = sqlx::query_scalar!(
        r#"
        SELECT mtr.api_key_purpose
        FROM model_traffic_rules mtr
        JOIN deployed_models dm ON dm.id = mtr.deployed_model_id
        WHERE dm.alias = $1
          AND dm.deleted = false
          AND mtr.api_key_purpose = $2
          AND mtr.action = 'deny'
        LIMIT 1
        "#,
        model_alias,
        purpose,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(found)
}

#[instrument(skip_all, name = "dwctl.get_user_id_of_api_key")]
pub async fn get_user_id_of_api_key(pool: PgPool, api_key: &str) -> Result<UserId, DbError> {
    let mut conn = pool.acquire().await?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    api_keys_repo
        .get_user_id_by_secret(api_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("API key not found or associated user doesn't exist").into())
}

/// Look up an API key's owning user and `purpose` by secret in a single query.
/// Returns `None` when the key does not exist or is deleted. Combining the two
/// lookups keeps the 403 enrichment path (and the Flex check) to one by-secret
/// query instead of fanning out.
#[instrument(skip_all, name = "dwctl.get_api_key_user_and_purpose")]
async fn get_api_key_user_and_purpose(pool: PgPool, api_key: &str) -> Result<Option<(UserId, String)>, DbError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query!(
        "SELECT user_id, purpose FROM api_keys WHERE secret = $1 AND is_deleted = false",
        api_key
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| (r.user_id, r.purpose)))
}

#[instrument(skip_all, name = "dwctl.get_balance_of_api_key")]
pub async fn get_balance_of_api_key(pool: PgPool, api_key: &str) -> Result<Decimal, DbError> {
    // Look up user_id from API key
    let user_id = get_user_id_of_api_key(pool.clone(), api_key).await?;

    debug!("Found user_id for API key: {}", user_id);

    // Query user's current balance
    let mut conn = pool.acquire().await?;
    let mut credits_repo = Credits::new(&mut conn);
    credits_repo.get_user_balance(user_id).await
}

#[instrument(skip_all, name = "dwctl.check_user_has_model_access")]
pub async fn check_user_has_model_access(pool: PgPool, user_id: UserId, model_alias: &str) -> Result<bool, DbError> {
    let mut conn = pool.acquire().await?;

    // Query to check if user has access to this deployment through group membership
    let result = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM deployed_models d
            JOIN deployment_groups dg ON dg.deployment_id = d.id
            WHERE d.alias = $1
              AND d.deleted = false
              AND dg.group_id IN (
                  SELECT ug.group_id FROM user_groups ug WHERE ug.user_id = $2
                  UNION
                  SELECT '00000000-0000-0000-0000-000000000000'::uuid
                  WHERE $2 != '00000000-0000-0000-0000-000000000000'
              )
        ) as "has_access!"
        "#,
        model_alias,
        user_id
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(result)
}

/// Validate that the bearer token's user is allowed to call the specified model.
///
/// Checks both group-based access and modality (traffic routing rule) restrictions.
/// Returns `Ok(())` if the request should be allowed, or a user-facing error message
/// if not.
///
/// Used by the inference middleware to fail fast on Flex requests that bypass
/// `onwards` entirely — without the modality check here, a Batch-purpose key
/// could send a Flex request and skip a deny rule onwards would have enforced.
pub async fn validate_api_key_model_access(pool: PgPool, api_key: &str, model: &str) -> Result<(), String> {
    // One by-secret lookup of the key's owner + purpose, reused below.
    let (user_id, purpose) = get_api_key_user_and_purpose(pool.clone(), api_key)
        .await
        .map_err(|_| "Invalid API key".to_string())?
        .ok_or_else(|| "Invalid API key".to_string())?;

    // Reject non-inference keys (e.g. `platform`) up front. Flex bypasses
    // onwards entirely, so this is the only place the purpose gate - mirrored
    // from `current_user` - runs for Flex requests. Live/realtime traffic is
    // covered by onwards (platform keys are excluded from its synced key set).
    // The system key (nil user) is purpose 'platform' but is used internally
    // for inference, so it is exempt here, mirroring the onwards key-sync
    // exemption.
    if user_id != uuid::Uuid::nil() && !crate::db::models::api_keys::is_inference_purpose(&purpose) {
        return Err(format!("API keys with purpose '{purpose}' cannot be used for inference requests."));
    }

    let has_access = check_user_has_model_access(pool.clone(), user_id, model)
        .await
        .map_err(|e| format!("Failed to check model access: {e}"))?;

    if !has_access {
        return Err(format!(
            "You do not have access to '{}'. Please contact your administrator to request access.",
            model
        ));
    }

    // Enforce the modality the Flex request will actually run as (`batch`), not
    // the caller's key purpose. Flex is dispatched via the owner's hidden batch
    // key, so a `realtime` deny rule (which exists to force traffic onto async)
    // must not block it — only a `batch` deny rule should, mirroring what
    // `onwards` enforces when the daemon later dispatches the request.
    if let Some(purpose) = check_modality_blocked_for_purpose(pool, model, FLEX_DISPATCH_PURPOSE)
        .await
        .map_err(|e| format!("Failed to check modality routing rules: {e}"))?
    {
        return Err(modality_blocked_message(&purpose, model));
    }

    Ok(())
}

/// Build the user-facing message returned when a routing rule denies the API
/// key's purpose for the requested model. Shared by the proxy enrichment
/// middleware and the Flex validation path so both surface identical wording.
fn modality_blocked_message(purpose: &str, model: &str) -> String {
    format!(
        "{} access to '{}' is blocked by a routing rule. Please contact your administrator to request access.",
        modality_label(purpose),
        model
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        handlers::{Credits, Repository as _, api_keys::ApiKeys},
        models::{
            api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose},
            credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
        },
    };
    use crate::{api::models::users::Role, test::utils::create_test_user};
    use rust_decimal::Decimal;

    /// Pure unit test: `modality_label` produces user-friendly labels for known
    /// purposes and capitalises unknown values rather than emitting a placeholder.
    #[test]
    fn test_modality_label_known_and_unknown_purposes() {
        assert_eq!(modality_label("batch"), "Batch");
        assert_eq!(modality_label("realtime"), "Real-time");
        assert_eq!(modality_label("playground"), "Playground");
        assert_eq!(modality_label("platform"), "Platform");

        // Unknown purposes capitalise the raw value so messages stay accurate
        // if a new purpose ships before this match is updated.
        assert_eq!(modality_label("evaluation"), "Evaluation");

        // Empty string falls back to a generic label rather than producing
        // a sentence starting with " access to ...".
        assert_eq!(modality_label(""), "This");
    }

    /// Integration test: Error enrichment middleware enriches 403 with balance info
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_enriches_403_with_balance(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // Create test user with an API key
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        // Give user some credits
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a test app with just the error enrichment middleware
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate onwards returning 403 (insufficient credits)
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Make request with API key in Authorization header
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Test that response is 403 and enriched with model access denied (user has positive balance but no model access)
        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(body.contains("do not have access to 'test-model'"));

        // Now, deduct all credits to simulate insufficient balance
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(10000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Usage".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // We need to ensure user is part of a group with access to the deployment to avoid 403
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id =
            crate::test::utils::create_test_model(&pool, "authorized-model-name", "authorized-model", endpoint_id, user.id).await;

        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        // Grant access to the group
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Make request with API key in Authorization header, using the model the user has access to
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "authorized-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Test that response is 402 and has been enriched
        assert_eq!(response.status_code().as_u16(), 402);
        let body = response.text();
        println!("Enriched response body: {}", body);
        assert!(body.contains("balance too low"));
    }

    /// Integration test: the spend-cap enrichment arms. Covers the 402
    /// `spend_cap_exceeded` envelope, the balance-supersedes-cap ordering,
    /// and the rolled-window fallthrough (no false "cap exceeded" during the
    /// post-boundary readmission lag).
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_spend_cap_arms(pool: PgPool) {
        use crate::db::handlers::api_keys::ApiKeys as ApiKeysRepo;
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // User with model access (so the model-access arm falls through) and
        // healthy balance (so the balance arm falls through).
        let user = create_test_user(&pool, Role::StandardUser).await;
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "cap-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "cap-model-name", "cap-model", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeysRepo::new(&mut conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Capped Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();
        drop(conn);

        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(credits_conn);

        // Cap the key at $10 (one-off) with an exhausted window.
        sqlx::query("UPDATE api_keys SET spend_limit = 10 WHERE id = $1")
            .bind(api_key.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_key_spend_checkpoints (api_key_id, total_spend, window_spend) VALUES ($1, 10.5, 10.5)")
            .bind(api_key.id)
            .execute(&pool)
            .await
            .unwrap();

        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));
        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let request = || {
            server
                .post("/ai/v1/chat/completions")
                .add_header("authorization", &format!("Bearer {}", api_key.secret))
                .json(&serde_json::json!({
                    "model": "cap-model",
                    "messages": [{"role": "user", "content": "Hello"}]
                }))
        };

        // 1. Healthy balance + exhausted cap → explicit 402 spend_cap_exceeded.
        let response = request().await;
        assert_eq!(response.status_code().as_u16(), 402);
        let body = response.text();
        assert!(body.contains("spend_cap_exceeded"), "expected cap code, got: {body}");
        assert!(body.contains("insufficient_quota"), "expected OpenAI quota type, got: {body}");
        assert!(body.contains("spending cap of $10"), "expected cap amount, got: {body}");

        // 2. Ordering: balance supersedes the cap. Blow the balance too — the
        //    user must see the balance error, not the cap error.
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(10000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Usage".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(credits_conn);
        let response = request().await;
        assert_eq!(response.status_code().as_u16(), 402);
        let body = response.text();
        assert!(body.contains("balance too low"), "balance must supersede the cap, got: {body}");
        assert!(!body.contains("spend_cap_exceeded"), "cap error must not surface, got: {body}");

        // 3. Post-boundary lag: restore the balance, make the cap windowed
        //    with a rolled-over (stale) window — exhausted counter, expired
        //    window. Until the sync readmits the key, requests still 403 at
        //    onwards; the enricher must NOT claim the cap is exceeded (the
        //    window has reset), must return the retriable 429 reset-pending
        //    response, and must fire the demand-driven boundary resync NOTIFY
        //    so the retry finds the key readmitted.
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(20000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Top-up".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(credits_conn);
        sqlx::query("UPDATE api_keys SET spend_limit_interval = 'daily' WHERE id = $1")
            .bind(api_key.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE api_key_spend_checkpoints SET window_started_at = now() - interval '2 days' WHERE api_key_id = $1")
            .bind(api_key.id)
            .execute(&pool)
            .await
            .unwrap();

        // Listener on the config channel to observe the boundary resync
        // notify (throttle reset so a parallel test can't have consumed it).
        crate::error_enrichment::reset_boundary_resync_throttle_for_tests();
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await.unwrap();
        listener.listen(crate::config::ONWARDS_CONFIG_CHANGED_CHANNEL).await.unwrap();
        while tokio::time::timeout(std::time::Duration::from_millis(10), listener.try_recv())
            .await
            .is_ok()
        {}

        let response = request().await;
        assert_eq!(
            response.status_code().as_u16(),
            429,
            "rolled window returns the retriable reset-pending response"
        );
        assert_eq!(
            response.headers().get("retry-after").and_then(|v| v.to_str().ok()),
            Some("5"),
            "reset-pending must carry the short notify-driven Retry-After"
        );
        let body = response.text();
        assert!(body.contains("spend_cap_reset_pending"), "expected reset-pending code, got: {body}");
        assert!(
            !body.contains("spend_cap_exceeded"),
            "must not claim an exceeded cap after the window rolled, got: {body}"
        );

        let notification = tokio::time::timeout(std::time::Duration::from_secs(2), listener.recv())
            .await
            .expect("timeout waiting for boundary resync notification")
            .expect("failed to receive notification");
        assert!(
            notification.payload().starts_with("api_key_spend_cap_boundary:"),
            "expected boundary resync payload, got: {}",
            notification.payload()
        );

        // Throttled: an immediate second failing request gets the same 429
        // but does not re-notify.
        let response = request().await;
        assert_eq!(response.status_code().as_u16(), 429);
        while let Ok(Ok(n)) = tokio::time::timeout(std::time::Duration::from_millis(500), listener.try_recv()).await {
            if let Some(n) = n {
                assert!(
                    !n.payload().starts_with("api_key_spend_cap_boundary:"),
                    "second request within the throttle window must not re-notify"
                );
            } else {
                break;
            }
        }
    }

    /// Integration test: Error enrichment middleware passes through 403 when user has access
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_passes_through_legitimate_403(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // Create test user with positive balance and model access
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add the user to it
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create API key
        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Test Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        // Give user positive balance
        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a deployment with 'authorized-model' alias and grant access to the group
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id =
            crate::test::utils::create_test_model(&pool, "authorized-model-name", "authorized-model", endpoint_id, user.id).await;

        // Grant access to the group
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Create a test app with middleware that returns 403 for a different reason (e.g., rate limit)
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate a legitimate 403 error (not credits/access related)
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Rate limit exceeded"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Make request with API key and a model the user has access to
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "authorized-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should pass through the original 403 without enrichment
        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(body.contains("Rate limit exceeded"));
        assert!(!body.contains("balance"));
        assert!(!body.contains("access"));
    }

    /// Integration test: Error enrichment middleware only affects /ai/ paths
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_ignores_non_ai_paths(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/admin/api/v1/users",
                axum::routing::get(|| async {
                    // Return 403 on admin path
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Admin Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server.get("/admin/api/v1/users").await;

        // Should remain 403, not enriched
        assert_eq!(response.status_code().as_u16(), 403);
        assert_eq!(response.text(), "Admin Forbidden");
    }

    /// Integration test: Error enrichment middleware ignores non-403 responses
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_ignores_non_403_errors(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Return 404 instead of 403
                    axum::response::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header("authorization", "Bearer dummy-key")
                        .body(axum::body::Body::from("Not Found"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", "Bearer test-key")
            .json(&serde_json::json!({"model": "test"}))
            .await;

        // Should remain 404, not enriched
        assert_eq!(response.status_code().as_u16(), 404);
        assert_eq!(response.text(), "Not Found");
    }

    /// Integration test: Error enrichment middleware enriches 403 when a routing
    /// rule denies the API key's purpose (e.g. batch/realtime/playground) for the model.
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_enriches_403_with_modality_block(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        // User with positive balance, model access, and a Batch-purpose API key.
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Batch Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Batch,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        let mut credits_conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut credits_conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(5000, 2),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Deploy a model the user can access via group membership.
        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "blocked-model-name", "blocked-model", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Add a deny rule for batch-purpose keys on this model.
        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Simulate onwards returning 403 because a deny rule matched.
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        let response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", api_key.secret))
            .json(&serde_json::json!({
                "model": "blocked-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(response.status_code().as_u16(), 403);
        let body = response.text();
        assert!(
            body.contains("Batch") && body.contains("blocked-model") && body.contains("administrator"),
            "expected modality-blocked message, got: {body}"
        );
    }

    /// Direct unit test: `validate_api_key_model_access` rejects a Flex request when a
    /// `batch` deny rule exists on the model. Flex requests on `/v1/responses` bypass
    /// onwards and execute as `batch` (via the owner's hidden batch key), so this
    /// function must fail them fast — otherwise a Flex request could reach a model where
    /// batch/async access is denied, which onwards would have blocked on dispatch.
    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_api_key_model_access_rejects_modality_blocked(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Batch Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Batch,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "blocked-model-name", "blocked-model", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = validate_api_key_model_access(pool.clone(), &api_key.secret, "blocked-model").await;
        let err = result.expect_err("expected modality-blocked rejection");
        assert!(
            err.contains("Batch") && err.contains("blocked-model") && err.contains("administrator"),
            "expected modality-blocked message, got: {err}"
        );

        // Sanity: a model without the deny rule still passes.
        let other_deployment_id = crate::test::utils::create_test_model(&pool, "open-model-name", "open-model", endpoint_id, user.id).await;
        add_deployment_to_group(&pool, other_deployment_id, group.id, user.id).await;
        validate_api_key_model_access(pool.clone(), &api_key.secret, "open-model")
            .await
            .expect("expected access to be granted on a model without a deny rule");
    }

    /// Regression test for the reported bug: a Flex (`service_tier:"flex"`) request
    /// submitted with a **realtime** key must NOT be blocked by a **realtime** deny
    /// rule. Such a rule exists to push traffic onto the async/Flex path, and Flex
    /// executes as `batch` — so `validate_api_key_model_access` (the Flex pre-dispatch
    /// gate) must allow it. Previously the check matched the caller's own key purpose
    /// and wrongly returned "Real-time access ... is blocked by a routing rule".
    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_flex_allowed_despite_realtime_deny_rule(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Realtime Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();
        drop(api_key_conn);

        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "async-only-name", "async-only", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        // Model denies realtime access (intended to force users onto async/Flex).
        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'realtime', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        validate_api_key_model_access(pool.clone(), &api_key.secret, "async-only")
            .await
            .expect("Flex request must be allowed despite a realtime-only deny rule");
    }

    /// Companion to the regression test: a Flex request submitted with a **realtime**
    /// key IS blocked when the model carries a **batch** deny rule, because Flex runs
    /// as `batch`. This mirrors what onwards would enforce on daemon dispatch.
    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_flex_blocked_by_batch_deny_rule_with_realtime_key(pool: PgPool) {
        use crate::test::utils::{add_deployment_to_group, add_user_to_group, create_test_group};

        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Realtime Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Realtime,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();
        drop(api_key_conn);

        let endpoint_id = crate::test::utils::create_test_endpoint(&pool, "test-endpoint", user.id).await;
        let deployment_id = crate::test::utils::create_test_model(&pool, "no-batch-name", "no-batch", endpoint_id, user.id).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;

        sqlx::query!(
            "INSERT INTO model_traffic_rules (deployed_model_id, api_key_purpose, action) VALUES ($1, 'batch', 'deny')",
            deployment_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = validate_api_key_model_access(pool.clone(), &api_key.secret, "no-batch")
            .await
            .expect_err("Flex request must be blocked by a batch deny rule (Flex runs as batch)");
        assert!(
            err.contains("Batch") && err.contains("no-batch") && err.contains("administrator"),
            "expected batch modality-blocked message, got: {err}"
        );
    }

    /// Direct unit test: `validate_api_key_model_access` rejects a non-inference
    /// (platform) key on the Flex path, before any model-access check.
    #[sqlx::test]
    async fn test_validate_api_key_model_access_rejects_platform_purpose(pool: PgPool) {
        let user = create_test_user(&pool, Role::StandardUser).await;

        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: user.id,
                name: "Platform Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Platform,
                requests_per_second: None,
                burst_size: None,
                created_by: user.id,
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        let result = validate_api_key_model_access(pool.clone(), &api_key.secret, "any-model").await;
        let err = result.expect_err("expected platform-purpose rejection");
        assert!(
            err.contains("platform") && err.contains("inference"),
            "expected non-inference purpose message, got: {err}"
        );
    }

    /// The system key is purpose 'platform' but is exempt from the inference
    /// purpose gate on the Flex path (it is used internally for inference),
    /// mirroring the onwards key-sync exemption.
    #[sqlx::test]
    async fn test_validate_api_key_model_access_exempts_system_key_from_purpose_gate(pool: PgPool) {
        let mut api_key_conn = pool.acquire().await.unwrap();
        let mut api_keys_repo = ApiKeys::new(&mut api_key_conn);
        let api_key = api_keys_repo
            .create(&ApiKeyCreateDBRequest {
                user_id: uuid::Uuid::nil(),
                name: "System Key".to_string(),
                description: None,
                purpose: ApiKeyPurpose::Platform,
                requests_per_second: None,
                burst_size: None,
                created_by: uuid::Uuid::nil(),
                spend_limit: None,
                spend_limit_interval: None,
            })
            .await
            .unwrap();

        // The purpose gate must NOT fire for the system (nil) user. A
        // model-access error is acceptable - we only assert the key was not
        // rejected on purpose grounds.
        if let Err(msg) = validate_api_key_model_access(pool.clone(), &api_key.secret, "any-model").await {
            assert!(
                !msg.contains("cannot be used for inference requests"),
                "system key must be exempt from the purpose gate, got: {msg}"
            );
        }
    }

    /// Integration test: Error enrichment middleware passes through when no auth header
    #[sqlx::test]
    #[test_log::test]
    async fn test_error_enrichment_middleware_without_auth_header(pool: PgPool) {
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    // Return 403 without auth header
                    axum::response::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("No Auth"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server
            .post("/ai/v1/chat/completions")
            .json(&serde_json::json!({"model": "test"}))
            .await;

        // Should pass through original 403 without enrichment
        assert_eq!(response.status_code().as_u16(), 403);
        assert_eq!(response.text(), "No Auth");
    }
}
