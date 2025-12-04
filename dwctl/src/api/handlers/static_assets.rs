//! HTTP handlers for static asset serving.

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri},
    response::{Html, IntoResponse},
};
use tracing::{debug, instrument};

use crate::static_assets;

/// Serve embedded static assets with SPA fallback
#[instrument]
pub async fn serve_embedded_asset(uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/');

    // If path is empty or ends with /, serve index.html
    if path.is_empty() || path.ends_with('/') {
        path = "index.html";
    }

    // Check filesystem first for specific volume-mounted files only
    // This avoids unnecessary filesystem checks for most static assets
    const FILESYSTEM_OVERRIDE_PATHS: &[&str] = &["bootstrap.js"];

    if FILESYSTEM_OVERRIDE_PATHS.contains(&path) {
        let fs_path = format!("/app/static/{}", path);
        if let Ok(content) = tokio::fs::read(&fs_path).await {
            let mime = mime_guess::from_path(path).first_or_octet_stream();

            return Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .header(axum::http::header::CACHE_CONTROL, "public, max-age=86400") // 24 hours
                .body(Body::from(content))
                .unwrap();
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

        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
            .header(axum::http::header::CACHE_CONTROL, cache_control)
            .body(Body::from(content.data.into_owned()))
            .unwrap();
    }

    // If not found, serve index.html for SPA client-side routing
    if let Some(index) = static_assets::Assets::get("index.html") {
        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/html")
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(Body::from(index.data.into_owned()))
            .unwrap();
    }

    // If even index.html is missing, return 404
    Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap()
}

/// SPA fallback handler - serves index.html for client-side routes
#[instrument(err)]
pub async fn spa_fallback(uri: Uri) -> Result<Html<String>, StatusCode> {
    debug!("Hitting SPA fallback for: {}", uri.path());

    // Serve embedded index.html
    if let Some(index) = static_assets::Assets::get("index.html") {
        let content = String::from_utf8_lossy(&index.data).to_string();
        Ok(Html(content))
    } else {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
    }
}
