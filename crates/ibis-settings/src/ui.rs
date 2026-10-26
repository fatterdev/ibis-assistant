//! Settings UI — the carried web SPA served from embedded static assets
//! (D13: embedded via `include_dir!`).
//!
//! served the directory with `http.FileServer(http.FS(web.FS))`
//! The Rust router exposes **relative** routes — `/` =
//! index.html, `/app.js`, `/style.css` — because `ibis-httpserver` nests it
//! with the `/settings/` prefix stripped (and the ephemeral server strips the
//! same prefix itself). Unknown paths answer the ServeMux 404 text.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use include_dir::{include_dir, Dir};

/// The embedded settings SPA.
pub static WEB: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/web");

fn serve_file(name: &'static str, content_type: &'static str) -> Response {
    match WEB.get_file(name) {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            file.contents(),
        )
            .into_response(),
        // Embedded at build time; unreachable in practice.
        None => plain_404(),
    }
}

/// Scenario: ServeMux/FileServer 404 body ( fallback / FileServer default).
pub(crate) fn plain_404() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        "404 page not found\n",
    )
        .into_response()
}

async fn index() -> Response {
    serve_file("index.html", "text/html; charset=utf-8")
}

async fn app_js() -> Response {
    serve_file("app.js", "text/javascript; charset=utf-8")
}

async fn style_css() -> Response {
    serve_file("style.css", "text/css; charset=utf-8")
}

/// The settings SPA router with relative paths (mounted by the mux under
/// `/settings/` with the prefix stripped, and by the ephemeral server the
/// same way).
pub fn settings_ui_router() -> Router {
    Router::new()
        .route("/", get(index))
        // the FileServer canonicalized /index.html to ./; serving the file
        // directly is behaviorally equivalent for browsers (D15 cosmetic).
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .fallback(|| async { plain_404() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn get_body(path: &str) -> (StatusCode, String, String) {
        let resp = settings_ui_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// The three embedded assets exist and are served with FileServer
    /// content types.
    #[tokio::test]
    async fn serves_embedded_spa() {
        let (status, ctype, body) = get_body("/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "text/html; charset=utf-8");
        assert!(body.contains("Ibis Assistant Settings"));

        let (status, ctype, _) = get_body("/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "text/javascript; charset=utf-8");

        let (status, ctype, _) = get_body("/style.css").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ctype, "text/css; charset=utf-8");

        let (status, ctype, body) = get_body("/nope.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.trim(), "404 page not found");
        assert_eq!(ctype, "text/plain; charset=utf-8");
    }
}
