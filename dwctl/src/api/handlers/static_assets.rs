//! HTTP handlers for static asset serving.

use axum::{
    body::Body,
    extract::State,
    http::{Response, StatusCode, Uri},
    response::{Html, IntoResponse},
};
use base64::Engine;
use tracing::{debug, error, instrument};

use crate::{AppState, static_assets};
use sqlx_pool_router::PoolProvider;

/// Default title used when no custom title is configured
const DEFAULT_TITLE: &str = "Doubleword Control Layer";

/// Inject the configured title into index.html content
fn inject_title(html: &str, title: Option<&str>) -> String {
    let title = title.unwrap_or(DEFAULT_TITLE);
    html.replace(&format!("<title>{}</title>", DEFAULT_TITLE), &format!("<title>{}</title>", title))
}

/// Serve embedded static assets with SPA fallback
#[instrument(skip(state))]
pub async fn serve_embedded_asset<P: PoolProvider + Clone>(State(state): State<AppState<P>>, uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/');

    // If path is empty or ends with /, serve index.html
    if path.is_empty() || path.ends_with('/') {
        path = "index.html";
    }

    // Check for bootstrap.js override via environment variable (base64 encoded)
    // This allows injecting custom bootstrap code without volume mounts
    if path == "bootstrap.js"
        && let Ok(encoded) = std::env::var("DASHBOARD_BOOTSTRAP_JS")
    {
        match base64::prelude::BASE64_STANDARD.decode(encoded.trim()) {
            Ok(content) => {
                debug!("Serving bootstrap.js from DASHBOARD_BOOTSTRAP_JS environment variable");
                return Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "text/javascript")
                    .header(axum::http::header::CACHE_CONTROL, "no-cache")
                    .body(Body::from(content))
                    .unwrap();
            }
            Err(e) => {
                error!("Failed to decode DASHBOARD_BOOTSTRAP_JS (expected base64): {}", e);
                // Fall through to embedded assets
            }
        }
    }

    // Fall back to embedded static assets
    if let Some(content) = static_assets::Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();

        // Set cache headers based on file path
        // Vite hashed assets can be cached indefinitely
        let cache_control = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            // HTML and other files should not be cached
            "no-cache"
        };

        // For index.html, inject the configured title
        if path == "index.html" {
            let html = String::from_utf8_lossy(&content.data);
            let html_with_title = inject_title(&html, state.config.metadata.title.as_deref());
            return Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .header(axum::http::header::CACHE_CONTROL, cache_control)
                .body(Body::from(html_with_title))
                .unwrap();
        }

        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
            .header(axum::http::header::CACHE_CONTROL, cache_control)
            .body(Body::from(content.data.into_owned()))
            .unwrap();
    }

    // If not found, serve index.html for SPA client-side routing
    if let Some(index) = static_assets::Assets::get("index.html") {
        let html = String::from_utf8_lossy(&index.data);
        let html_with_title = inject_title(&html, state.config.metadata.title.as_deref());
        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/html")
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(Body::from(html_with_title))
            .unwrap();
    }

    // If even index.html is missing, return 404
    Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap()
}

/// SPA fallback handler - serves index.html for client-side routes
#[instrument(skip(state), err)]
pub async fn spa_fallback<P: PoolProvider + Clone>(State(state): State<AppState<P>>, uri: Uri) -> Result<Html<String>, StatusCode> {
    debug!("Hitting SPA fallback for: {}", uri.path());

    // Serve embedded index.html with injected title
    if let Some(index) = static_assets::Assets::get("index.html") {
        let html = String::from_utf8_lossy(&index.data);
        let html_with_title = inject_title(&html, state.config.metadata.title.as_deref());
        Ok(Html(html_with_title))
    } else {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
    }
}
