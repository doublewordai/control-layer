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
//! - a **dashboard session** — the console renders each `dw-img://` token in
//!   a request body as a link to this endpoint, so a user can open it to see
//!   the original image they submitted; or
//! - a **`platform`-purpose API key** (Bearer) for programmatic callers.
//!
//! Anything that follows redirects (a browser `<img>`, `curl -L`, an HTTP
//! client) then fetches the bytes from the returned signed URL.
//!
//! Authorisation: the caller must either have submitted a request referencing
//! this image (a matching `user_id` row in `image_access`) or be acting in the
//! organization it was submitted under (org-scoped keys also record
//! `organization_id`). A personal submission stays visible only to the
//! submitter — never to an organization. Content-addressed deduplication means
//! many users can share the same hash; each grant is authorised independently.
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

    // Authorise: the caller must have access to this hash, which is either
    //   * they are the acting user who submitted it (`user_id`), OR
    //   * they are acting in the organization the image was submitted under
    //     (`organization_id` = their membership-validated active org).
    // A personal submission has `organization_id = NULL`, so it never matches
    // the org branch and stays visible only to the submitter.
    //
    // Use the PRIMARY pool — the access row is written by the realtime
    // middleware / batch ingest path which also uses the primary, so any
    // immediate "view what I just submitted" lookup avoids replica lag.
    let sha_bytes: Vec<u8> = token.0.to_vec();
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let authorized = is_authorized_to_view(&mut conn, &sha_bytes, current_user.id, current_user.active_organization)
        .await
        .map_err(|e: sqlx::Error| Error::Database(e.into()))?;
    drop(conn);

    if !authorized {
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

/// Who an image submission is attributed to, mirroring how `CurrentUser` is
/// derived from an API key: the acting human (`user_id`) and, for organization
/// API keys, the owning organization (`organization_id`). Personal keys leave
/// `organization_id` as `None`.
#[derive(Debug, Clone, Copy)]
pub struct ImageAttribution {
    /// The acting human — the API key's `created_by`.
    pub user_id: uuid::Uuid,
    /// The owning organization, set only for org-scoped keys; `None` for
    /// personal submissions.
    pub organization_id: Option<uuid::Uuid>,
}

/// Resolve an API key secret to its image attribution, using the same rule as
/// `CurrentUser`: the acting user is the key's `created_by`, and for org keys
/// (`created_by <> user_id`) the organization is the key's `user_id`. Returns
/// `None` if the key is unknown/deleted.
pub async fn resolve_image_attribution(pool: &sqlx::PgPool, api_key: &str) -> Option<ImageAttribution> {
    let row = sqlx::query!(
        r#"
        SELECT created_by AS "created_by!", user_id AS "user_id!"
        FROM api_keys
        WHERE secret = $1 AND is_deleted = FALSE
        LIMIT 1
        "#,
        api_key,
    )
    .fetch_optional(pool)
    .await
    .ok()??;

    let organization_id = (row.created_by != row.user_id).then_some(row.user_id);
    Some(ImageAttribution {
        user_id: row.created_by,
        organization_id,
    })
}

/// Record that `attribution` submitted a request containing `token`. Idempotent
/// on `(user_id, sha256)`: updates `last_seen_at` on conflict, and preserves an
/// existing org grant (`COALESCE`) so a later personal submission of the same
/// image by the same user does not revoke organization visibility.
///
/// Best-effort: errors are logged and swallowed. We never block the request
/// path on this bookkeeping write — the security control (substituting the
/// URL before forwarding to the upstream) does not depend on it.
pub async fn record_image_access(pool: &sqlx::PgPool, attribution: ImageAttribution, token: ImageToken, mime: &str, bytes_len: u64) {
    let sha_bytes: Vec<u8> = token.0.to_vec();
    let bytes_len_i64 = bytes_len as i64;
    if let Err(e) = sqlx::query!(
        r#"
        INSERT INTO image_access (user_id, organization_id, sha256, mime, bytes_len, first_seen_at, last_seen_at)
        VALUES ($1, $2, $3, $4, $5, NOW(), NOW())
        ON CONFLICT (user_id, sha256) DO UPDATE
        SET last_seen_at = NOW(),
            mime = EXCLUDED.mime,
            bytes_len = EXCLUDED.bytes_len,
            organization_id = COALESCE(EXCLUDED.organization_id, image_access.organization_id)
        "#,
        attribution.user_id,
        attribution.organization_id,
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

/// Whether `viewer` — optionally acting in organization `active_org` — is
/// authorized to view the image identified by `sha256`. True when the viewer
/// submitted it (`user_id`) OR they are acting in the organization it was
/// submitted under (`organization_id`). A personal submission has
/// `organization_id = NULL`, so it never matches the org branch and is visible
/// only to the submitter. `active_org` is already membership-validated when it
/// reaches us (see `CurrentUser`).
async fn is_authorized_to_view(
    conn: &mut sqlx::PgConnection,
    sha256: &[u8],
    viewer: uuid::Uuid,
    active_org: Option<uuid::Uuid>,
) -> std::result::Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT 1 AS "exists!"
        FROM image_access
        WHERE sha256 = $1
          AND (user_id = $2 OR organization_id = $3)
        LIMIT 1
        "#,
        sha256,
        viewer,
        active_org,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::api_keys::ApiKeyCreate;
    use crate::api::models::users::Role;
    use crate::db::handlers::Organizations;
    use crate::db::handlers::api_keys::ApiKeys;
    use crate::db::handlers::repository::Repository;
    use crate::db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose};
    use crate::test::utils::{create_test_api_key_for_user, create_test_org, create_test_user};
    use sqlx::PgPool;

    fn token(b: u8) -> ImageToken {
        ImageToken([b; 32])
    }

    async fn can_view(pool: &PgPool, tok: ImageToken, viewer: uuid::Uuid, active_org: Option<uuid::Uuid>) -> bool {
        let mut conn = pool.acquire().await.unwrap();
        is_authorized_to_view(&mut conn, &tok.0.to_vec(), viewer, active_org).await.unwrap()
    }

    /// Create an org-scoped API key (user_id = org, created_by = member) and return its secret.
    async fn create_org_api_key(pool: &PgPool, org_id: uuid::Uuid, member_id: uuid::Uuid) -> String {
        let mut conn = pool.acquire().await.unwrap();
        let create = ApiKeyCreate {
            name: format!("Org key {}", uuid::Uuid::new_v4().simple()),
            description: None,
            purpose: ApiKeyPurpose::Realtime,
            requests_per_second: None,
            burst_size: None,
            member_id: None,
            spend_limit: None,
            spend_limit_interval: None,
        };
        let req = ApiKeyCreateDBRequest::new(org_id, member_id, create);
        ApiKeys::new(&mut conn).create(&req).await.unwrap().secret
    }

    #[sqlx::test]
    async fn personal_image_is_private_org_image_is_org_visible(pool: PgPool) {
        let p = create_test_user(&pool, Role::StandardUser).await; // submits personally
        let q = create_test_user(&pool, Role::StandardUser).await; // submits under the org key
        let org = create_test_org(&pool, p.id).await; // p is owner/member of org
        {
            let mut conn = pool.acquire().await.unwrap();
            Organizations::new(&mut conn).add_member(org.id, q.id, "member").await.unwrap();
        }

        let personal = token(1);
        let org_img = token(2);
        record_image_access(
            &pool,
            ImageAttribution {
                user_id: p.id,
                organization_id: None,
            },
            personal,
            "image/png",
            10,
        )
        .await;
        record_image_access(
            &pool,
            ImageAttribution {
                user_id: q.id,
                organization_id: Some(org.id),
            },
            org_img,
            "image/png",
            20,
        )
        .await;

        // Personal submission: only the submitter; never the org.
        assert!(can_view(&pool, personal, p.id, None).await, "P views own personal image");
        assert!(
            can_view(&pool, personal, p.id, Some(org.id)).await,
            "P views own personal even in org context"
        );
        assert!(!can_view(&pool, personal, q.id, None).await, "Q cannot view P's personal image");
        assert!(
            !can_view(&pool, personal, q.id, Some(org.id)).await,
            "a personal image must never be visible via the org"
        );

        // Org submission: the submitter always; other org members when acting
        // in the org; never a non-member. (`active_org` is membership-validated
        // before it reaches the handler, so we only pass legitimate values.)
        assert!(
            can_view(&pool, org_img, q.id, None).await,
            "submitter sees own org image without org context too"
        );
        assert!(
            can_view(&pool, org_img, q.id, Some(org.id)).await,
            "submitter sees own org image in org context"
        );
        assert!(
            can_view(&pool, org_img, p.id, Some(org.id)).await,
            "another org member (in org context) sees it"
        );
        assert!(
            !can_view(&pool, org_img, p.id, None).await,
            "org member WITHOUT org context does not see it"
        );
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        assert!(
            !can_view(&pool, org_img, outsider.id, None).await,
            "a non-member never sees the org image"
        );
    }

    #[sqlx::test]
    async fn resolve_attribution_distinguishes_personal_and_org_keys(pool: PgPool) {
        // Personal key: created_by == user_id -> no org.
        let person = create_test_user(&pool, Role::StandardUser).await;
        let personal_key = create_test_api_key_for_user(&pool, person.id).await;
        let attr = resolve_image_attribution(&pool, &personal_key.secret).await.expect("known key");
        assert_eq!(attr.user_id, person.id);
        assert_eq!(attr.organization_id, None);

        // Org key: created_by (member) != user_id (org).
        let member = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, member.id).await;
        let org_secret = create_org_api_key(&pool, org.id, member.id).await;
        let attr = resolve_image_attribution(&pool, &org_secret).await.expect("known org key");
        assert_eq!(attr.user_id, member.id, "acting user is the member (created_by)");
        assert_eq!(attr.organization_id, Some(org.id), "org is the key owner (user_id)");

        assert!(resolve_image_attribution(&pool, "sk-does-not-exist").await.is_none());
    }
}
