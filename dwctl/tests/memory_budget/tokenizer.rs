//! Mock tokenizer service for the prompt cache. It lists the model and declines
//! every render or tokenize call, which the cache treats as "do not cache".

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;

pub struct Tokenizer {
    pub base_url: String,
}

impl Tokenizer {
    pub async fn start(alias: &str) -> Tokenizer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock tokenizer");
        let addr = listener.local_addr().unwrap();
        let models = json!({
            "models": [{ "alias": alias, "hf_repo": "test/model", "tokenizer_version": "v1", "template_version": "t1" }]
        });
        let router = Router::new()
            .route("/v1/models", get(move || async move { axum::Json(models) }))
            .fallback(|| async { (StatusCode::UNPROCESSABLE_ENTITY, "UNMAPPED_MODEL").into_response() });
        tokio::spawn(async move { axum::serve(listener, router).await.expect("mock tokenizer server") });
        Tokenizer {
            base_url: format!("http://{addr}"),
        }
    }
}
