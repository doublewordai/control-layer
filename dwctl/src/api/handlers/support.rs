//! HTTP handlers for support request endpoints.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use utoipa::ToSchema;

use crate::{
    AppState,
    api::models::users::CurrentUser,
    email_jobs::SendEmailInput,
    errors::{Error, Result},
};

/// Per-user submission cap inside [`SUPPORT_REQUEST_WINDOW_SECS`].
///
/// Defends against a single account (script, accidental retry loop, or
/// compromised credentials) draining the shared transactional-email
/// allowance. The limit is intentionally generous for a human reporting an
/// issue and tight enough that automated abuse trips it quickly.
const SUPPORT_REQUEST_LIMIT: i64 = 5;

/// Sliding-window length in seconds for the [`SUPPORT_REQUEST_LIMIT`] cap.
const SUPPORT_REQUEST_WINDOW_SECS: u64 = 3600;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SupportRequest {
    /// Subject line for the support request
    pub subject: String,
    /// Message body
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SupportResponse {
    /// Whether the support request was accepted for delivery. Accepted does
    /// not mean delivered — the actual send runs asynchronously via the
    /// `send-email` worker, which retries transient provider failures.
    pub sent: bool,
}

/// Submit a support request via email.
///
/// Enqueues the send rather than awaiting it inline so the caller doesn't
/// see a 5xx when the upstream provider is unavailable or rate-limiting.
/// The actual delivery runs in the `send-email` worker with retry on
/// transient errors.
///
/// Per-user rate limit: [`SUPPORT_REQUEST_LIMIT`] accepted submissions per
/// [`SUPPORT_REQUEST_WINDOW_SECS`] window. Excess submissions get HTTP 429
/// with a `Retry-After` header. The limit is enforced via a row count
/// against the `support_request_submissions` audit table so it applies
/// uniformly across replicas.
#[utoipa::path(
    post,
    path = "/support/requests",
    request_body = SupportRequest,
    responses(
        (status = 200, description = "Support request accepted for delivery", body = SupportResponse),
        (status = 400, description = "Subject or message missing"),
        (status = 429, description = "Per-user submission rate limit exceeded"),
        (status = 500, description = "Failed to enqueue support request"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = [])),
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn submit_support_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Json(request): Json<SupportRequest>,
) -> Result<Json<SupportResponse>> {
    let subject = request.subject.trim();
    let message = request.message.trim();
    let config = state.current_config();

    if subject.is_empty() || message.is_empty() {
        return Err(Error::BadRequest {
            message: "Subject and message are required".to_string(),
        });
    }

    // Rate-limit check + audit-row insert in a single transaction so the
    // count and the new row stay consistent under concurrent submissions
    // from the same user. The audit row is what the next request's COUNT
    // query observes; without the same transaction the second of two
    // concurrent requests could read a stale count and slip past the cap.
    let window_secs = SUPPORT_REQUEST_WINDOW_SECS as i32;
    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let recent_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM support_request_submissions
         WHERE user_id = $1
           AND created_at > NOW() - make_interval(secs => $2::int)",
        current_user.id,
        window_secs,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Database(e.into()))?
    .unwrap_or(0);

    if recent_count >= SUPPORT_REQUEST_LIMIT {
        // Don't insert the audit row on rejection — the row records accepted
        // submissions, not attempts. Rolling back the empty transaction is
        // free; the explicit drop is documentation.
        drop(tx);
        return Err(Error::TooManyRequests {
            message: format!("support request limit reached ({SUPPORT_REQUEST_LIMIT} per hour); please wait before submitting again"),
            retry_after_seconds: SUPPORT_REQUEST_WINDOW_SECS,
        });
    }

    sqlx::query!("INSERT INTO support_request_submissions (user_id) VALUES ($1)", current_user.id,)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(e.into()))?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    // Enqueue after the audit row is committed. If the enqueue fails the
    // caller sees 500 and the audit row remains; on retry they'll be one
    // step closer to the limit. That's acceptable — enqueue failures are
    // pool-down events, not steady state.
    state
        .task_runner
        .send_email_job
        .enqueue(&SendEmailInput::SupportRequest {
            support_email: config.support_email.clone(),
            user_email: current_user.email.clone(),
            user_name: current_user.display_name.clone(),
            subject: subject.to_string(),
            message: message.to_string(),
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("enqueue support request: {e}"),
        })?;

    Ok(Json(SupportResponse { sent: true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_app, create_test_user};
    use serde_json::json;
    use sqlx::PgPool;

    /// Helper: POST a support request body, returning the axum-test response.
    async fn post_support_request(
        server: &axum_test::TestServer,
        headers: &[(String, String)],
        subject: &str,
        message: &str,
    ) -> axum_test::TestResponse {
        let mut req = server
            .post("/admin/api/v1/support/requests")
            .json(&json!({ "subject": subject, "message": message }));
        for (k, v) in headers {
            req = req.add_header(k, v);
        }
        req.await
    }

    #[sqlx::test]
    async fn submit_succeeds_and_records_audit_row(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let headers = add_auth_headers(&user);

        let resp = post_support_request(&server, &headers, "Help me", "I can't log in").await;
        resp.assert_status_ok();

        let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM support_request_submissions WHERE user_id = $1", user.id,)
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(count, 1, "exactly one audit row per accepted submission");
    }

    #[sqlx::test]
    async fn rejects_after_limit_with_429_and_retry_after(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let headers = add_auth_headers(&user);

        // Fill the bucket.
        for i in 0..SUPPORT_REQUEST_LIMIT {
            let resp = post_support_request(&server, &headers, &format!("subj {i}"), "body").await;
            resp.assert_status_ok();
        }

        // The next one must be rejected.
        let resp = post_support_request(&server, &headers, "one too many", "body").await;
        resp.assert_status(axum::http::StatusCode::TOO_MANY_REQUESTS);

        let retry_after = resp
            .headers()
            .get("retry-after")
            .expect("Retry-After header on 429")
            .to_str()
            .expect("Retry-After header is ascii")
            .to_string();
        assert_eq!(retry_after, SUPPORT_REQUEST_WINDOW_SECS.to_string());

        // No audit row recorded for the rejection.
        let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM support_request_submissions WHERE user_id = $1", user.id,)
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(count, SUPPORT_REQUEST_LIMIT, "rejection must not insert an audit row");
    }

    #[sqlx::test]
    async fn limit_is_per_user(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user_a = create_test_user(&pool, Role::StandardUser).await;
        let user_b = create_test_user(&pool, Role::StandardUser).await;
        let headers_a = add_auth_headers(&user_a);
        let headers_b = add_auth_headers(&user_b);

        // User A fills their bucket.
        for _ in 0..SUPPORT_REQUEST_LIMIT {
            post_support_request(&server, &headers_a, "subj", "body").await.assert_status_ok();
        }
        post_support_request(&server, &headers_a, "blocked", "body")
            .await
            .assert_status(axum::http::StatusCode::TOO_MANY_REQUESTS);

        // User B is unaffected.
        post_support_request(&server, &headers_b, "first for B", "body")
            .await
            .assert_status_ok();
    }

    #[sqlx::test]
    async fn submissions_outside_window_dont_count(pool: PgPool) {
        let (server, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let headers = add_auth_headers(&user);

        // Seed SUPPORT_REQUEST_LIMIT old rows just outside the window. The
        // handler should NOT see them in its count and should accept the
        // next submission.
        for _ in 0..SUPPORT_REQUEST_LIMIT {
            sqlx::query!(
                "INSERT INTO support_request_submissions (user_id, created_at)
                 VALUES ($1, NOW() - make_interval(secs => $2::int))",
                user.id,
                (SUPPORT_REQUEST_WINDOW_SECS as i32) + 60,
            )
            .execute(&pool)
            .await
            .unwrap();
        }

        post_support_request(&server, &headers, "should pass", "body")
            .await
            .assert_status_ok();
    }
}
