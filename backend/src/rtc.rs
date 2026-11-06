use axum::{
    extract::{Query, State},
    Json,
};
use serde::Serialize;
use std::collections::HashMap;

use crate::AppState;

#[derive(Serialize)]
pub struct RtcConfig {
    pub iceServers: Vec<serde_json::Value>,
}

pub async fn rtc_config(
    State(_state): State<AppState>,
    Query(_q): Query<HashMap<String, String>>,
) -> Json<RtcConfig> {
    let stun: Vec<String> = std::env::var("STUN_URLS")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302,stun:stun1.l.google.com:19302".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let turn = std::env::var("TURN_URLS").unwrap_or_default();
    let mut iceServers: Vec<serde_json::Value> = stun
        .iter()
        .map(|u| serde_json::json!({ "urls": u }))
        .collect();
    if !turn.is_empty() {
        iceServers.push(serde_json::json!({
            "urls": turn.split(',').map(|s| s.trim().to_string()).collect::<Vec<_>>(),
            "username": std::env::var("TURN_USERNAME").unwrap_or_default(),
            "credential": std::env::var("TURN_CREDENTIAL").unwrap_or_default(),
        }));
    }
    Json(RtcConfig { iceServers })
}
