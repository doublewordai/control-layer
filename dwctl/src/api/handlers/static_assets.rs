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

    // Try to serve the requested file
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, Router};
    use axum_test::TestServer;

    fn create_test_router() -> Router {
        Router::new().fallback(serve_embedded_asset)
    }

    #[tokio::test]
    async fn test_serve_root_returns_index_html() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/").await;

        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("text/html")
        );
        assert_eq!(
            response.headers().get("cache-control").map(|v| v.to_str().unwrap()),
            Some("no-cache")
        );

        let text = response.text();
        assert!(text.contains("<!doctype html>") || text.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn test_serve_index_html_explicitly() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/index.html").await;

        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("text/html")
        );

        let text = response.text();
        assert!(text.contains("<!doctype html>") || text.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn test_serve_favicon() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/favicon.svg").await;

        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("image/svg+xml")
        );
        assert_eq!(
            response.headers().get("cache-control").map(|v| v.to_str().unwrap()),
            Some("no-cache")
        );
    }

    #[tokio::test]
    async fn test_hashed_assets_have_immutable_cache() {
        // Test that files not under /assets/ have no-cache headers
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/mockServiceWorker.js").await;

        // This file exists and should have no-cache (not under /assets/)
        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("cache-control").map(|v| v.to_str().unwrap()),
            Some("no-cache")
        );
        assert!(response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap())
            .unwrap()
            .contains("javascript"));
    }

    #[tokio::test]
    async fn test_spa_fallback_for_unknown_routes() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        // Request a client-side route that doesn't exist as a file
        let response = server.get("/dashboard/users/123").await;

        // Should serve index.html for SPA routing
        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("text/html")
        );

        let text = response.text();
        assert!(text.contains("<!doctype html>") || text.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn test_spa_fallback_handler_directly() {
        let uri = "/some/client/route".parse().unwrap();
        let result = spa_fallback(uri).await;

        assert!(result.is_ok());
        let html = result.unwrap();
        let content = html.0;
        assert!(content.contains("<!doctype html>") || content.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn test_trailing_slash_serves_index() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/dashboard/").await;

        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("text/html")
        );
    }

    #[tokio::test]
    async fn test_serve_png_file() {
        let app = create_test_router();
        let server = TestServer::new(app).unwrap();

        let response = server.get("/favicon.png").await;

        response.assert_status(StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").map(|v| v.to_str().unwrap()),
            Some("image/png")
        );
    }
}
