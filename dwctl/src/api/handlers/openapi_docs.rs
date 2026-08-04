//! Handlers that serve the OpenAPI specs and Scalar doc UIs.
//!
//! Both surfaces require authentication. The Admin surface additionally
//! requires an admin-level identity (PlatformManager role, the admin
//! user, or a `platform`-purpose API key) — the spec describes the full
//! internal management API and would otherwise hand attackers a map of
//! the privileged surface.

use axum::{
    Json,
    response::{Html, IntoResponse, Response},
};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::{
    api::models::users::CurrentUser,
    auth::permissions::{RequiresPermission, operation, resource},
    openapi::{AdminApiDoc, AiApiDoc},
};

/// Pinned Scalar renderer bundle. utoipa-scalar's default template loads
/// the UNVERSIONED `https://cdn.jsdelivr.net/npm/@scalar/api-reference`,
/// which floats to whatever Scalar last published and cannot be
/// subresource-integrity-checked. We pin the exact version and hash
/// instead.
///
/// Deployments that enforce a Content-Security-Policy (via a fronting
/// proxy or `auth.security.headers`) should allowlist this exact URL in
/// `script-src` rather than the whole CDN host. CSP Level 2 host-sources
/// match the full path (exact match for paths not ending in `/`), so this
/// works in all modern browsers — but only because this versioned URL is
/// served directly: CSP skips path matching after a redirect, and the
/// unversioned `@scalar/api-reference` URL redirects. If your CSP pins
/// the verbatim URL, bump it in the same change as this constant or the
/// docs pages render blank.
const SCALAR_JS_URL: &str = "https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.64.0/dist/browser/standalone.js";
/// SRI hash of [`SCALAR_JS_URL`]. Recompute on version bumps:
/// `curl -sL <url> | openssl dgst -sha384 -binary | base64`
const SCALAR_JS_SRI: &str = "sha384-ei8P62VHbV+6AdLO3hN333PsTEYp6k9OAVhlYvpmer+zdPIf8jSbdgh9ojiLWX3T";

/// The stock utoipa-scalar template (`res/scalar.html` in the crate) with
/// the script tag swapped for the pinned + integrity-checked bundle.
/// `$spec` is substituted by [`Scalar::to_html`]; it is an inert JSON data
/// block, so it needs no CSP allowance.
fn scalar_html(title: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
    <title>{title}</title>
    <meta charset="utf-8"/>
    <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script id="api-reference" type="application/json">
    $spec
</script>
<script src="{SCALAR_JS_URL}" integrity="{SCALAR_JS_SRI}" crossorigin="anonymous"></script>
</body>
</html>"#
    )
}

/// Serve the Admin OpenAPI spec as JSON.
///
/// `RequiresPermission<System, ReadAll>` is the admin-or-PlatformManager
/// gate: `is_admin` bypasses, PlatformManager is granted (System is not
/// Requests), and StandardUser / RequestViewer are denied.
#[tracing::instrument(skip_all)]
pub async fn admin_openapi_json(_: RequiresPermission<resource::System, operation::ReadAll>) -> Json<utoipa::openapi::OpenApi> {
    Json(AdminApiDoc::openapi())
}

/// Serve the Scalar UI for the Admin OpenAPI spec.
#[tracing::instrument(skip_all)]
pub async fn admin_openapi_docs(_: RequiresPermission<resource::System, operation::ReadAll>) -> Response {
    let scalar = Scalar::with_url("/admin/openapi.json", AdminApiDoc::openapi()).custom_html(scalar_html("Admin API"));
    Html(scalar.to_html()).into_response()
}

/// Serve the AI OpenAPI spec as JSON. Any authenticated identity may read it.
#[tracing::instrument(skip_all)]
pub async fn ai_openapi_json(_: CurrentUser) -> Json<utoipa::openapi::OpenApi> {
    Json(AiApiDoc::openapi())
}

/// Serve the Scalar UI for the AI OpenAPI spec.
#[tracing::instrument(skip_all)]
pub async fn ai_openapi_docs(_: CurrentUser) -> Response {
    let scalar = Scalar::with_url("/ai/openapi.json", AiApiDoc::openapi()).custom_html(scalar_html("AI API"));
    Html(scalar.to_html()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The docs pages must load the renderer from the exact pinned URL —
    /// deployment CSPs may allowlist it verbatim — and `$spec` must have
    /// been substituted with the inline spec.
    #[test]
    fn scalar_html_renders_pinned_bundle_and_inline_spec() {
        let html = Scalar::with_url("/ai/openapi.json", AiApiDoc::openapi())
            .custom_html(scalar_html("AI API"))
            .to_html();

        assert!(html.contains(&format!(
            r#"src="{SCALAR_JS_URL}" integrity="{SCALAR_JS_SRI}" crossorigin="anonymous""#
        )));
        assert!(!html.contains("$spec"));
        assert!(html.contains(r#""openapi""#));
        // The unversioned floating URL must not sneak back in.
        assert!(!html.contains(r#"src="https://cdn.jsdelivr.net/npm/@scalar/api-reference""#));
    }
}
