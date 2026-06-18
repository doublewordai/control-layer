//! Regression guard for the compiled-in default CORS allowlist.
//!
//! The default must contain a valid `http://localhost:3001` origin (the Vite dev
//! frontend). A scheme typo such as `htt://localhost:3001` still parses as a URL
//! but silently never matches a real browser `Origin`, so local-dev CORS would
//! break with no error. This is an integration test (links only the public lib)
//! so it compiles against the offline SQLx cache without needing a database.

use dwctl::config::{CorsConfig, CorsOrigin};

#[test]
fn default_cors_origins_include_http_localhost_vite_frontend() {
    let cfg = CorsConfig::default();
    let has_vite_origin = cfg.allowed_origins.iter().any(|origin| {
        matches!(
            origin,
            CorsOrigin::Url(url)
                if url.scheme() == "http"
                    && url.host_str() == Some("localhost")
                    && url.port() == Some(3001)
        )
    });
    assert!(
        has_vite_origin,
        "default CORS allowlist should include http://localhost:3001; got {:?}",
        cfg.allowed_origins
    );
}
