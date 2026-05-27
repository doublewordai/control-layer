use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::api::models::provider_display_configs::{
    CreateProviderDisplayConfig, ProviderDisplayConfigResponse, UpdateProviderDisplayConfig,
};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::ProviderDisplayConfigs;
use crate::db::models::provider_display_configs::{ProviderDisplayConfigCreateDBRequest, ProviderDisplayConfigUpdateDBRequest};
use crate::errors::{Error, Result};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use url::Url;

fn normalize_provider_key(value: &str) -> String {
    value.trim().to_lowercase()
}

fn validate_provider_key(provider_key: &str) -> Result<()> {
    if provider_key.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "provider_key must not be empty".to_string(),
        });
    }
    Ok(())
}

fn validate_icon(icon: Option<&str>) -> Result<()> {
    let Some(icon) = icon.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };

    let is_builtin = matches!(icon, "anthropic" | "google" | "openai" | "onwards" | "snowflake");
    let is_url = icon.starts_with("https://") || icon.starts_with("/");
    if !is_builtin && !is_url {
        return Err(Error::BadRequest {
            message: "icon must be an https URL, root-relative asset path, or built-in icon key".to_string(),
        });
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/provider-display-configs",
    tag = "provider-display-configs",
    summary = "List provider display configs",
    responses(
        (status = 200, description = "Provider display configs", body = Vec<ProviderDisplayConfigResponse>)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_provider_display_configs<P: PoolProvider>(
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Json<Vec<ProviderDisplayConfigResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);

    let known = repo.list_known_providers().await?;
    let configs = repo.list().await?;

    let config_map: HashMap<_, _> = configs.into_iter().map(|config| (config.provider_key.clone(), config)).collect();
    let known_map: HashMap<_, _> = known
        .into_iter()
        .map(|provider| (provider.provider_key.clone(), provider))
        .collect();

    let mut keys = BTreeMap::new();
    for key in config_map.keys() {
        keys.insert(key.clone(), ());
    }
    for key in known_map.keys() {
        keys.insert(key.clone(), ());
    }

    let mut response = Vec::new();
    for key in keys.into_keys() {
        response.push(ProviderDisplayConfigResponse::from_parts(
            config_map.get(&key).cloned(),
            known_map.get(&key).cloned(),
        ));
    }

    response.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Get provider display config",
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 200, description = "Provider display config", body = ProviderDisplayConfigResponse),
        (status = 404, description = "Provider display config not found")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Json<ProviderDisplayConfigResponse>> {
    let provider_key = normalize_provider_key(&provider_key);
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo.get_by_key(&provider_key).await?;
    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    match (config, known) {
        (None, None) => Err(Error::NotFound {
            resource: "provider display config".to_string(),
            id: provider_key,
        }),
        (config, known) => Ok(Json(ProviderDisplayConfigResponse::from_parts(config, known))),
    }
}

#[utoipa::path(
    post,
    path = "/provider-display-configs",
    tag = "provider-display-configs",
    summary = "Create provider display config",
    request_body = CreateProviderDisplayConfig,
    responses(
        (status = 201, description = "Provider display config created", body = ProviderDisplayConfigResponse)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(create): Json<CreateProviderDisplayConfig>,
) -> Result<(StatusCode, Json<ProviderDisplayConfigResponse>)> {
    let provider_key = normalize_provider_key(&create.provider_key);
    validate_provider_key(&provider_key)?;
    validate_icon(create.icon.as_deref())?;

    let display_name = create
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(create.provider_key.trim())
        .to_string();

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo
        .create(&ProviderDisplayConfigCreateDBRequest {
            provider_key: provider_key.clone(),
            display_name,
            icon: create.icon.filter(|value| !value.trim().is_empty()),
            created_by: current_user.id,
        })
        .await?;

    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    Ok((
        StatusCode::CREATED,
        Json(ProviderDisplayConfigResponse::from_parts(Some(config), known)),
    ))
}

#[utoipa::path(
    patch,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Update provider display config",
    request_body = UpdateProviderDisplayConfig,
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 200, description = "Provider display config updated", body = ProviderDisplayConfigResponse)
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn update_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::UpdateAll>,
    Json(update): Json<UpdateProviderDisplayConfig>,
) -> Result<Json<ProviderDisplayConfigResponse>> {
    let provider_key = normalize_provider_key(&provider_key);
    validate_provider_key(&provider_key)?;
    validate_icon(update.icon.as_ref().and_then(|value| value.as_deref()))?;

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let config = repo
        .update(
            &provider_key,
            &ProviderDisplayConfigUpdateDBRequest {
                display_name: update.display_name.and_then(|value| {
                    let trimmed = value.trim().to_string();
                    (!trimmed.is_empty()).then_some(trimmed)
                }),
                icon: update.icon.map(|value| {
                    value.and_then(|icon| {
                        let trimmed = icon.trim().to_string();
                        (!trimmed.is_empty()).then_some(trimmed)
                    })
                }),
            },
        )
        .await?;

    let known = repo
        .list_known_providers()
        .await?
        .into_iter()
        .find(|provider| provider.provider_key == provider_key);

    Ok(Json(ProviderDisplayConfigResponse::from_parts(Some(config), known)))
}

#[utoipa::path(
    delete,
    path = "/provider-display-configs/{provider_key}",
    tag = "provider-display-configs",
    summary = "Delete provider display config",
    params(
        ("provider_key" = String, Path)
    ),
    responses(
        (status = 204, description = "Provider display config deleted")
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_provider_display_config<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::UpdateAll>,
) -> Result<StatusCode> {
    let provider_key = normalize_provider_key(&provider_key);
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ProviderDisplayConfigs::new(&mut conn);
    let deleted = repo.delete(&provider_key).await?;
    if !deleted {
        return Err(Error::NotFound {
            resource: "provider display config".to_string(),
            id: provider_key,
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- Icon proxy ----------

/// Cap on bytes returned from the upstream icon URL. Icons are SVG/PNG logos;
/// anything larger is either a misconfiguration or an upstream that returned
/// something unexpected (HTML error page, etc.) — better to fail closed.
const MAX_ICON_SIZE_BYTES: u64 = 2 * 1024 * 1024;

/// Total time (connect + read) we'll spend talking to the upstream icon host
/// before giving up. Provider logos are tiny static files; if the upstream
/// can't deliver in this window, it's not going to.
const ICON_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// `Cache-Control` returned to the browser. Provider icons effectively never
/// change at a given URL — `staleTime` on the SPA's TanStack Query already
/// keeps the list response cached for 30 minutes, so an hour here is conservative.
const ICON_CACHE_CONTROL: &str = "public, max-age=3600";

/// Returns `false` for any IP we don't want the icon proxy to reach:
/// RFC1918 (`10.*`, `172.16/12`, `192.168.*`), loopback, link-local
/// (including the cloud metadata address `169.254.169.254`), CGNAT
/// (`100.64/10`, also Tailscale's range), IPv6 unique-local (`fc00::/7`)
/// and link-local (`fe80::/10`), and the various "reserved" / documentation
/// ranges. Built from stable [`Ipv4Addr`] / [`Ipv6Addr`] predicates plus
/// hand-rolled bit checks for ranges whose helpers are still nightly-only
/// (`is_shared`, `is_unique_local`, `is_unicast_link_local`, etc).
fn is_globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_globally_routable(v4),
        // Treat IPv4-mapped IPv6 as IPv4 — without this, `::ffff:127.0.0.1`
        // would slip past the IPv6 checks and resolve to loopback.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => ipv4_is_globally_routable(v4),
            None => ipv6_is_globally_routable(v6),
        },
    }
}

fn ipv4_is_globally_routable(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
    {
        return false;
    }
    let [a, b, _, _] = ip.octets();
    // RFC6598 CGNAT (`100.64.0.0/10`). Cluster overlays (incl. Tailscale)
    // sit in this range; `Ipv4Addr::is_shared` is still nightly-only.
    if a == 100 && (0x40..=0x7f).contains(&b) {
        return false;
    }
    // IETF Protocol Assignments `192.0.0.0/24`.
    if a == 192 && b == 0 && ip.octets()[2] == 0 {
        return false;
    }
    // Benchmarking `198.18.0.0/15`.
    if a == 198 && (b == 18 || b == 19) {
        return false;
    }
    // Reserved class E `240.0.0.0/4` (255.255.255.255 already filtered).
    if a >= 240 {
        return false;
    }
    true
}

fn ipv6_is_globally_routable(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let s = ip.segments();
    // Unique-local `fc00::/7` (RFC4193) — `is_unique_local` is nightly-only.
    if (s[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // Link-local `fe80::/10` — `is_unicast_link_local` is nightly-only.
    if (s[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // Documentation `2001:db8::/32`.
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false;
    }
    // Discard prefix `100::/64`.
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
        return false;
    }
    true
}

/// DNS resolver used by [`icon_http_client`]. Drops any address that
/// [`is_globally_routable`] rejects.
///
/// Without this filter the icon proxy is an SSRF gadget: even though an
/// operator picks the `icon` URL, a compromised provider host or a hostile
/// DNS response can resolve to an address inside the cluster network and let
/// an unprivileged caller pull data from the host. Filtering at the resolver
/// (rather than parsing the URL) catches CNAMEs, A-record changes, and
/// multi-record responses where only one entry is internal.
#[derive(Debug, Default)]
struct SsrfSafeResolver;

impl reqwest::dns::Resolve for SsrfSafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // Port 0 — reqwest substitutes the URL's port before connecting.
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let filtered: Vec<SocketAddr> = resolved.filter(|sa| is_globally_routable(sa.ip())).collect();
            if filtered.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("no globally routable address for {host}"),
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(filtered.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Shared HTTP client for the icon proxy. Built once on first use so that
/// the connection pool, TLS state, and DNS resolver are reused across
/// requests — re-building per request would defeat all three caches and
/// drop a fresh `SsrfSafeResolver` instance every call.
fn icon_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(ICON_FETCH_TIMEOUT)
            // Redirects disabled: the SSRF resolver only inspects the
            // initial host. A 302 from an upstream into the internal
            // network would bypass it, so refuse to follow them at all.
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(SsrfSafeResolver))
            // Several icon hosts (Wikimedia in particular, per its
            // User-Agent policy) return 403 for requests with no
            // User-Agent, so identify ourselves.
            .user_agent(concat!(
                "dwctl-icon-proxy/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/doublewordai/control-layer)",
            ))
            .build()
            .expect("icon proxy reqwest client builds with a static config")
    })
}

/// Server-side proxy for operator-set provider icon URLs.
///
/// `provider_display_configs.icon` is set by admins through the admin surface
/// and currently points at arbitrary external hosts (npmmirror, simpleicons,
/// Wikipedia, GitHub avatars, Webflow CDNs, ...) that change each time a new
/// provider is added. Routing the dashboard's icon loads through this endpoint
/// means the browser only ever fetches icons from the same origin, so the SPA
/// can run with a tight `img-src 'self' data: blob:` CSP rather than allowing
/// every external image host that an admin might paste in.
///
/// Returns:
///   - **200** with the upstream bytes and `Content-Type` (must be `image/*`)
///   - **404** if the provider has no config, no icon set, or the configured
///     value isn't an absolute `https://` URL (relative paths and registry-key
///     shortcuts are handled client-side and bypass this endpoint)
///   - **500** if the upstream fetch fails, times out, exceeds 2 MiB, returns
///     non-2xx, or serves a non-image Content-Type
#[utoipa::path(
    get,
    path = "/provider-display-configs/{provider_key}/icon",
    tag = "provider-display-configs",
    summary = "Proxy the provider's configured icon",
    description = "Fetches the operator-set icon URL server-side and streams it back. Lets the SPA keep a tight CSP regardless of where the icon URL points.",
    params(("provider_key" = String, Path)),
    responses(
        (status = 200, description = "Icon bytes"),
        (status = 404, description = "Provider or icon not found / not proxyable"),
        (status = 500, description = "Upstream icon fetch failed"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all, fields(provider_key = %provider_key))]
pub async fn get_provider_display_config_icon<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(provider_key): Path<String>,
    _: RequiresPermission<resource::Models, operation::ReadOwn>,
) -> Result<Response> {
    let provider_key = normalize_provider_key(&provider_key);

    let icon_value = {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut repo = ProviderDisplayConfigs::new(&mut conn);
        repo.get_by_key(&provider_key)
            .await?
            .ok_or_else(|| Error::NotFound {
                resource: "provider display config".to_string(),
                id: provider_key.clone(),
            })?
            .icon
            .unwrap_or_default()
    };
    // Trim — operator-pasted values regularly have trailing whitespace,
    // and `Url::parse` rejects them.
    let icon_value = icon_value.trim();

    // Only proxy absolute https:// URLs. Relative paths (`/brand/...`) and
    // registry-key shortcuts (`anthropic`, `google`) are resolved by the SPA
    // and don't need (or want) to round-trip through this endpoint.
    let icon_url = match Url::parse(icon_value) {
        Ok(u) if u.scheme() == "https" => u,
        _ => {
            return Err(Error::NotFound {
                resource: "proxyable icon for provider".to_string(),
                id: provider_key,
            });
        }
    };

    let resp = icon_http_client()
        .get(icon_url)
        .send()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("fetch provider icon: {e}")))?;

    let upstream_status = resp.status();
    if !upstream_status.is_success() {
        return Err(Error::Other(anyhow::anyhow!(
            "upstream icon host returned status {upstream_status}",
        )));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Refuse to proxy anything that isn't an image — guards against an
    // upstream returning an HTML error page that the browser would then try
    // to render in an <img> tag.
    if !content_type.to_ascii_lowercase().starts_with("image/") {
        return Err(Error::Other(anyhow::anyhow!(
            "upstream icon content-type {content_type:?} is not image/*",
        )));
    }

    let body_bytes = read_capped_body(resp, MAX_ICON_SIZE_BYTES)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("read provider icon body: {e}")))?;

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, ICON_CACHE_CONTROL)
        .body(Body::from(body_bytes))
        .map_err(|e| Error::Other(anyhow::anyhow!("build icon response: {e}")))
}

/// Drain a `reqwest::Response` body, returning early with an error if the
/// accumulated bytes would exceed `max`. Avoids buffering an unbounded body
/// from an untrusted upstream.
async fn read_capped_body(mut resp: reqwest::Response, max: u64) -> anyhow::Result<bytes::Bytes> {
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = resp.chunk().await? {
        if (buf.len() as u64) + (chunk.len() as u64) > max {
            anyhow::bail!("icon body exceeds {max} bytes");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

#[cfg(test)]
mod ssrf_filter_tests {
    use super::is_globally_routable;
    use std::net::IpAddr;

    fn check(s: &str) -> bool {
        is_globally_routable(s.parse::<IpAddr>().unwrap())
    }

    #[test]
    fn allows_public_v4() {
        assert!(check("8.8.8.8"));
        assert!(check("1.1.1.1"));
        assert!(check("142.250.190.78")); // google.com sample
    }

    #[test]
    fn allows_public_v6() {
        assert!(check("2606:4700:4700::1111")); // cloudflare
        assert!(check("2001:4860:4860::8888")); // google
    }

    #[test]
    fn blocks_rfc1918() {
        assert!(!check("10.0.0.1"));
        assert!(!check("10.255.255.255"));
        assert!(!check("172.16.0.1"));
        assert!(!check("172.31.255.255"));
        assert!(!check("192.168.1.1"));
    }

    #[test]
    fn blocks_loopback() {
        assert!(!check("127.0.0.1"));
        assert!(!check("127.255.255.254"));
        assert!(!check("::1"));
    }

    #[test]
    fn blocks_link_local_and_metadata() {
        assert!(!check("169.254.169.254")); // AWS / GCP / Azure metadata
        assert!(!check("169.254.0.1"));
        assert!(!check("fe80::1"));
    }

    #[test]
    fn blocks_cgnat() {
        // RFC6598 100.64.0.0/10 — also Tailscale's range.
        assert!(!check("100.64.0.1"));
        assert!(!check("100.127.255.254"));
        // Just outside the /10 — must remain allowed.
        assert!(check("100.63.255.254"));
        assert!(check("100.128.0.1"));
    }

    #[test]
    fn blocks_unspecified_and_broadcast() {
        assert!(!check("0.0.0.0"));
        assert!(!check("255.255.255.255"));
        assert!(!check("::"));
    }

    #[test]
    fn blocks_multicast() {
        assert!(!check("224.0.0.1"));
        assert!(!check("239.255.255.255"));
        assert!(!check("ff02::1"));
    }

    #[test]
    fn blocks_reserved_class_e_and_documentation() {
        assert!(!check("240.0.0.1"));
        assert!(!check("254.0.0.1"));
        assert!(!check("192.0.2.1")); // TEST-NET-1
        assert!(!check("198.51.100.1")); // TEST-NET-2
        assert!(!check("203.0.113.1")); // TEST-NET-3
        assert!(!check("198.18.0.1")); // benchmarking
        assert!(!check("192.0.0.1")); // IETF protocol assignments
        assert!(!check("2001:db8::1")); // IPv6 documentation
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        // fc00::/7
        assert!(!check("fc00::1"));
        assert!(!check("fdff:ffff::1"));
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6_loopback() {
        // ::ffff:127.0.0.1 should be treated as 127.0.0.1.
        assert!(!check("::ffff:127.0.0.1"));
        assert!(!check("::ffff:10.0.0.1"));
        assert!(!check("::ffff:169.254.169.254"));
    }
}

#[cfg(test)]
mod tests {
    use crate::api::models::provider_display_configs::ProviderDisplayConfigResponse;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use sqlx::PgPool;

    /// Helper: create a deployed model with provider metadata and return its ID
    async fn create_model_with_provider(pool: &PgPool, alias: &str, provider: &str, created_by: uuid::Uuid) -> uuid::Uuid {
        let endpoint_id = get_test_endpoint_id(pool).await;
        let deployment_id = uuid::Uuid::new_v4();
        sqlx::query!(
            r#"
            INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by, deleted, metadata)
            VALUES ($1, $2, $3, $4, $5, false, $6)
            "#,
            deployment_id,
            alias,
            alias,
            endpoint_id,
            created_by,
            serde_json::json!({ "provider": provider }),
        )
        .execute(pool)
        .await
        .expect("Failed to create test model with provider");
        deployment_id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_can_read_all_provider_display_configs(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create one deployed provider and one config-only provider.
        let anthropic_model = create_model_with_provider(&pool, "claude-3", "Anthropic", admin.id).await;
        let group = create_test_group(&pool).await;
        add_deployment_to_group(&pool, anthropic_model, group.id, admin.id).await;
        add_user_to_group(&pool, user.id, group.id).await;
        sqlx::query!(
            r#"
            INSERT INTO provider_display_configs (provider_key, display_name, icon, created_by)
            VALUES ($1, $2, $3, $4)
            "#,
            "openai",
            "OpenAI",
            Some("openai"),
            admin.id
        )
        .execute(&pool)
        .await
        .expect("Failed to create provider display config");

        // Standard users should be able to read both deployed and config-only providers.
        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"anthropic"), "Should see deployed provider");
        assert!(provider_keys.contains(&"openai"), "Should see config-only provider");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_sees_all_providers(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create models for two different providers (not assigned to any group)
        create_model_with_provider(&pool, "claude-3", "Anthropic", admin.id).await;
        create_model_with_provider(&pool, "gpt-4", "OpenAI", admin.id).await;

        // Admin should see both providers regardless of group membership
        let headers = add_auth_headers(&admin);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"anthropic"), "Admin should see Anthropic");
        assert!(provider_keys.contains(&"openai"), "Admin should see OpenAI");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_sees_everyone_group_providers(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a model and add it to the Everyone group (nil UUID)
        let model_id = create_model_with_provider(&pool, "gemini-pro", "Google", admin.id).await;
        let everyone_group_id = uuid::Uuid::nil();
        add_deployment_to_group(&pool, model_id, everyone_group_id, admin.id).await;

        // Standard user should see the provider via the Everyone group
        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/provider-display-configs")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status_ok();
        let providers: Vec<ProviderDisplayConfigResponse> = response.json();
        let provider_keys: Vec<&str> = providers.iter().map(|p| p.provider_key.as_str()).collect();
        assert!(provider_keys.contains(&"google"), "Should see provider from Everyone group");
    }
}
