//! Image-view endpoint.
//!
//! `GET /admin/api/v1/images/:sha256` returns a short-lived signed URL
//! pointing at the bytes for the requested content hash. Used by the
//! dashboard to render images that the user submitted via `data:` URI
//! (or HTTP URL) and that have been normalised into the content store.
//!
//! ## Resolving `dw-img://` tokens
//!
//! When image-input normalization is enabled, stored request bodies
//! reference images by an opaque `dw-img://<sha256>` token rather than the
//! original URL or inline base64. To get the original bytes back for a
//! token, call this endpoint with the token's `<sha256>`:
//!
//! ```text
//! GET /admin/api/v1/images/<sha256>
//! ```
//!
//! This lives on the dwctl-native management API, **not** the
//! OpenAI-compatible `/ai` surface (which has no equivalent concept). It
//! accepts the same credentials as the rest of the management API:
//!
//! - a **dashboard session** — the console renders `dw-img://` tokens as
//!   `<img>` automatically, so users see their original images; or
//! - a **`platform`-purpose API key** (Bearer) for programmatic callers.
//!
//! Anything that follows redirects (a browser `<img>`, `curl -L`, an HTTP
//! client) then fetches the bytes from the returned signed URL.
//!
//! Authorisation: the user must have a row in `image_access` for this
//! sha256, meaning they previously submitted a request that referenced
//! this image. Content-addressed deduplication means many users can
//! share the same hash; each is authorised independently.
//!
//! The endpoint returns a 302 redirect to a signed URL (browser follows
//! it natively for `<img src>` use). Returning 302 keeps dwctl off the
//! egress path for the actual bytes.

use crate::AppState;
use crate::api::models::users::CurrentUser;
use crate::errors::{Error, Result};
use crate::image_normalizer::{ImageToken, TokenParseError};
use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header::LOCATION},
    response::{IntoResponse, Response},
};
use sqlx_pool_router::PoolProvider;
use std::time::Duration;
use tracing::warn;

/// `GET /admin/api/v1/images/:sha256`
///
/// 302 redirects to a short-lived signed URL for the bytes. Returns 404
/// when the requesting user has no recorded access to this hash (we don't
/// distinguish "not found" from "exists but you can't access" — leaking
/// existence is the SSRF-style problem we're avoiding).
#[tracing::instrument(skip_all)]
pub async fn get_image<P: PoolProvider + Clone + Send + Sync>(
    State(state): State<AppState<P>>,
    current_user: CurrentUser,
    Path(sha256_hex): Path<String>,
) -> Result<Response> {
    let token: ImageToken = sha256_hex.parse().map_err(|e: TokenParseError| Error::BadRequest {
        message: format!("invalid image hash: {e}"),
    })?;

    let config = state.current_config();
    if !config.image_normalizer.enabled {
        // Feature disabled: no point looking up.
        return Err(Error::NotFound {
            resource: "image".to_string(),
            id: sha256_hex,
        });
    }

    // Authorise: this user must have a row in image_access for this hash.
    // Use the PRIMARY pool — the access row is written by the realtime
    // middleware / batch ingest path which also uses the primary, so any
    // immediate "view what I just submitted" lookup avoids replica lag.
    let sha_bytes: Vec<u8> = token.0.to_vec();
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let row = sqlx::query!(
        r#"
        SELECT 1 AS "exists!"
        FROM image_access
        WHERE user_id = $1 AND sha256 = $2
        LIMIT 1
        "#,
        current_user.id,
        sha_bytes,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| Error::Database(e.into()))?;
    drop(conn);

    if row.is_none() {
        return Err(Error::NotFound {
            resource: "image".to_string(),
            id: sha256_hex,
        });
    }

    // Use the AppState-bound normaliser singleton (built once at startup).
    // Re-creating it per request would re-init the GCS client + ADC signer
    // on every dashboard image load.
    let ttl = Duration::from_secs(config.image_normalizer.signing.dashboard_ttl_secs);
    let signed = state.image_normalizer.sign(token, ttl).await.map_err(|e| {
        warn!(error = %e, "image_normalizer.sign failed for dashboard view");
        Error::Internal {
            operation: format!("image signing failed: {e}"),
        }
    })?;

    let mut response = (StatusCode::FOUND, "").into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&signed.url).map_err(|e| Error::Internal {
            operation: format!("invalid signed URL: {e}"),
        })?,
    );
    // Don't cache the redirect — the signed URL expires.
    response
        .headers_mut()
        .insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-store, private"));
    Ok(response)
}

/// Record that `user_id` submitted a request containing `token`. Idempotent:
/// updates `last_seen_at` on conflict so we keep a useful liveness signal
/// for any future garbage collection / dedup-stats query.
///
/// Best-effort: errors are logged and swallowed. We never block the request
/// path on this bookkeeping write — the security control (substituting the
/// URL before forwarding to the upstream) does not depend on it.
pub async fn record_image_access(pool: &sqlx::PgPool, user_id: uuid::Uuid, token: ImageToken, mime: &str, bytes_len: u64) {
    let sha_bytes: Vec<u8> = token.0.to_vec();
    let bytes_len_i64 = bytes_len as i64;
    if let Err(e) = sqlx::query!(
        r#"
        INSERT INTO image_access (user_id, sha256, mime, bytes_len, first_seen_at, last_seen_at)
        VALUES ($1, $2, $3, $4, NOW(), NOW())
        ON CONFLICT (user_id, sha256) DO UPDATE
        SET last_seen_at = NOW(),
            mime = EXCLUDED.mime,
            bytes_len = EXCLUDED.bytes_len
        "#,
        user_id,
        sha_bytes,
        mime,
        bytes_len_i64,
    )
    .execute(pool)
    .await
    {
        warn!(error = %e, "failed to record image_access row (non-fatal)");
    }
}
