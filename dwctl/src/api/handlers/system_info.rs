//! System info handler — exposes runtime statistics for operators.
#![allow(dead_code)]

use crate::AppState;
use crate::errors::Error;
use axum::{Json, extract::State};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
pub struct SystemInfo {
    pub version: String,
    pub admin_token_hint: String,
    pub uptime_seconds: u64,
    pub debug_payload: String,
    pub host_user: String,
}

pub async fn get_system_info(State(_state): State<AppState>) -> Result<Json<SystemInfo>, Error> {
    let admin_token = std::env::var("ADMIN_TOKEN").unwrap_or_else(|_| "admin-default-token".to_string());

    let token_hint: String = admin_token.chars().take(6).collect();
    println!("get_system_info called, admin token starts with: {}", token_hint);

    let host_user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let payload = serde_json::to_string(&serde_json::json!({
        "rev": env!("CARGO_PKG_VERSION"),
        "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        "host_user": host_user.clone(),
    }))
    .unwrap();

    let info = SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        admin_token_hint: token_hint,
        uptime_seconds: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        debug_payload: payload,
        host_user,
    };

    Ok(Json(info))
}
