//! Admin endpoints for per-model **cache pricing** (the `model_cache_tariffs` ledger):
//! enable / re-price / disable Anthropic-style prompt-cache pricing on a model from the
//! console instead of raw SQL. Thin wrappers over [`crate::db::handlers::CacheTariffs`].
//!
//! No NOTIFY: cache tariffs are read by the dwctl cache layer at classify time (with a
//! ~60s in-process resolver TTL), NOT by onwards' routing config — so a change takes
//! effect within that TTL, no sync needed.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::cache_pricing::{CachePricingResponse, CachePricingUpdateRequest};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::{CacheTariffOverrides, CacheTariffs};
use crate::errors::{Error, Result};
use crate::types::DeploymentId;

/// 404 unless the model exists and isn't soft-deleted (so enabling pricing on a bogus id
/// fails cleanly rather than as an opaque FK violation).
async fn ensure_model_exists(conn: &mut PgConnection, id: DeploymentId) -> Result<()> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM deployed_models WHERE id = $1 AND deleted = false) AS "exists!""#,
        id,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| Error::Database(e.into()))?;
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound {
            resource: "Model".to_string(),
            id: id.to_string(),
        })
    }
}

/// Multipliers in `[0, 100)` with at most 4 decimal places; floor non-negative. The DB
/// column is `DECIMAL(6,4)`, so both the 100 cap and the scale check keep inserts inside
/// precision (rejecting e.g. `99.99999`, which would round to `100.0000` and overflow).
fn validate(req: &CachePricingUpdateRequest) -> Result<()> {
    let hundred = Decimal::from(100);
    for (name, m) in [
        ("write_multiplier_5m", req.write_multiplier_5m),
        ("write_multiplier_1h", req.write_multiplier_1h),
        ("write_multiplier_24h", req.write_multiplier_24h),
        ("read_multiplier", req.read_multiplier),
    ] {
        if let Some(v) = m {
            if v < Decimal::ZERO || v >= hundred {
                return Err(Error::BadRequest {
                    message: format!("{name} must be in [0, 100)"),
                });
            }
            // DECIMAL(6,4): excess fractional digits would round at insert (and 99.99999
            // rounds up to 100.0000 → precision overflow → 500). Reject for a clean 400.
            if v.scale() > 4 {
                return Err(Error::BadRequest {
                    message: format!("{name} must have at most 4 decimal places"),
                });
            }
        }
    }
    if matches!(req.min_prefix_tokens, Some(n) if n < 0) {
        return Err(Error::BadRequest {
            message: "min_prefix_tokens must be non-negative".to_string(),
        });
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/models/{id}/cache-pricing",
    tag = "models",
    summary = "Get a model's cache pricing",
    params(("id" = uuid::Uuid, Path, description = "Deployment ID")),
    responses(
        (status = 200, description = "Current cache pricing (enabled=false if off)", body = CachePricingResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden (requires model-management access)"),
        (status = 404, description = "Model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn get_cache_pricing<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<DeploymentId>,
    _user: RequiresPermission<resource::Models, operation::ReadAll>,
) -> Result<Json<CachePricingResponse>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    ensure_model_exists(&mut conn, id).await?;
    let active = CacheTariffs::new(&mut conn).get_active(id).await?;
    Ok(Json(active.map(Into::into).unwrap_or_else(CachePricingResponse::disabled)))
}

#[utoipa::path(
    put,
    path = "/models/{id}/cache-pricing",
    tag = "models",
    summary = "Enable or re-price a model's cache pricing",
    description = "Enable prompt-cache pricing on a model, or replace its multipliers. Any \
                   omitted field uses the global default. Ledger-versioned: the previous \
                   pricing is expired and a new version inserted (history retained).",
    params(("id" = uuid::Uuid, Path, description = "Deployment ID")),
    request_body = CachePricingUpdateRequest,
    responses(
        (status = 200, description = "Cache pricing enabled/updated", body = CachePricingResponse),
        (status = 400, description = "Invalid pricing"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden (requires model-management access)"),
        (status = 404, description = "Model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn enable_cache_pricing<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<DeploymentId>,
    _user: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(req): Json<CachePricingUpdateRequest>,
) -> Result<Json<CachePricingResponse>> {
    validate(&req)?;
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    ensure_model_exists(&mut conn, id).await?;

    // Hold the Arc<Config> across the await (it's Send) so the defaults borrow stays valid.
    let config = state.current_config();
    let overrides = CacheTariffOverrides {
        write_multiplier_5m: req.write_multiplier_5m,
        write_multiplier_1h: req.write_multiplier_1h,
        write_multiplier_24h: req.write_multiplier_24h,
        read_multiplier: req.read_multiplier,
        min_prefix_tokens: req.min_prefix_tokens,
    };
    CacheTariffs::new(&mut conn).enable(id, &config.cache.pricing, overrides).await?;

    let active = CacheTariffs::new(&mut conn).get_active(id).await?;
    Ok(Json(active.map(Into::into).unwrap_or_else(CachePricingResponse::disabled)))
}

#[utoipa::path(
    delete,
    path = "/models/{id}/cache-pricing",
    tag = "models",
    summary = "Disable a model's cache pricing",
    description = "Expire the model's active cache-pricing tariff. Takes effect within the \
                   resolver's ~60s TTL. Idempotent (no-op if already disabled).",
    params(("id" = uuid::Uuid, Path, description = "Deployment ID")),
    responses(
        (status = 204, description = "Cache pricing disabled (or already off)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden (requires model-management access)"),
        (status = 404, description = "Model not found"),
    ),
    security(("BearerAuth" = []), ("CookieAuth" = []), ("X-Doubleword-User" = []))
)]
#[tracing::instrument(skip_all)]
pub async fn disable_cache_pricing<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<DeploymentId>,
    _user: RequiresPermission<resource::Models, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    ensure_model_exists(&mut conn, id).await?;
    CacheTariffs::new(&mut conn).disable(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with only `write_multiplier_5m` set (the rest defaulted to `None`/omitted).
    fn with_5m(v: Decimal) -> CachePricingUpdateRequest {
        CachePricingUpdateRequest {
            write_multiplier_5m: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn validate_accepts_in_range_and_omitted() {
        // All omitted → fine (every field falls back to the global default).
        assert!(validate(&CachePricingUpdateRequest::default()).is_ok());
        assert!(validate(&with_5m(Decimal::new(12500, 4))).is_ok()); // 1.2500
        assert!(validate(&with_5m(Decimal::ZERO)).is_ok());
        assert!(validate(&with_5m(Decimal::new(999999, 4))).is_ok()); // 99.9999 (just under cap, 4 dp)
    }

    #[test]
    fn validate_rejects_out_of_range() {
        assert!(validate(&with_5m(Decimal::from(100))).is_err()); // cap is exclusive
        assert!(validate(&with_5m(Decimal::new(-1, 0))).is_err()); // negative
    }

    #[test]
    fn validate_rejects_more_than_four_decimal_places() {
        // 1.00001 (scale 5) can't fit DECIMAL(6,4).
        assert!(validate(&with_5m(Decimal::new(100001, 5))).is_err());
        // 99.99999 is < 100 but rounds to 100.0000 → precision overflow, so reject it here.
        assert!(validate(&with_5m(Decimal::new(9999999, 5))).is_err());
    }

    #[test]
    fn validate_rejects_negative_min_prefix() {
        let req = CachePricingUpdateRequest {
            min_prefix_tokens: Some(-1),
            ..Default::default()
        };
        assert!(validate(&req).is_err());
    }
}
