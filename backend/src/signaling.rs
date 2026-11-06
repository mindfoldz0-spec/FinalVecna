use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::{AppState, auth, calls::{Call, CallStatus}, db};

#[derive(Deserialize)]
pub struct CreateCallBody {
    pub dialed_number: String,
}

#[derive(Serialize)]
pub struct CreateCallResponse {
    pub call_id: String,
    pub dialed_number: String,
    pub call_token: String,
}

fn valid_dialed(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_digit() || c == '*' || c == '#')
}

pub async fn create_call(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<CreateCallBody>,
) -> Result<Json<CreateCallResponse>, StatusCode> {
    if !valid_dialed(body.dialed_number.trim()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let call = state.calls.create(body.dialed_number.trim().to_string());
    let token = auth::sign_call_token(&state.call_token_secret, &call.call_id);
    let ip = addr.ip().to_string();
    crate::rate_check("calls", addr.ip())?;
    db::record_call_created(&state.db, &call.call_id, &call.dialed_number, Some(&ip)).await;
    broadcast_agents(
        &state,
        &serde_json::json!({ "event": "NEW_CALL", "call": call }),
    )
    .await;
    Ok(Json(CreateCallResponse {
        call_id: call.call_id,
        dialed_number: call.dialed_number,
        call_token: token,
    }))
}

pub(crate) async fn broadcast_agents(state: &AppState, msg: &serde_json::Value) {
    use dashmap::mapref::entry::Entry;
    let text = msg.to_string();
    let mut dead: Vec<(String, Arc<tokio::sync::mpsc::UnboundedSender<String>>)> = Vec::new();
    for entry in state.calls.agent_sockets.iter() {
        if entry.value().send(text.clone()).is_err() {
            dead.push((entry.key().clone(), entry.value().clone()));
        }
    }
    // Only remove an entry if it's still the same (dead) sender — a live
    // reconnect may have registered a fresh one in the meantime.
    for (id, tx) in dead {
        if let Entry::Occupied(e) = state.calls.agent_sockets.entry(id) {
            if Arc::ptr_eq(e.get(), &tx) {
                e.remove();
            }
        }
    }
}

async fn send_caller(state: &AppState, call_id: &str, msg: &serde_json::Value) {
    if let Some(tx) = state.calls.caller_sockets.get(call_id) {
        let _ = tx.send(msg.to_string());
    }
}

/// Caller socket: /ws/caller/{call_id}?token=xxx
pub async fn caller_ws(
    State(state): State<AppState>,
    Path(call_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = q.get("token").cloned().unwrap_or_default();
    if !auth::verify_call_token(&state.call_token_secret, &call_id, &token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state.calls.get(&call_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| handle_caller(socket, state, call_id))
        .into_response()
}

async fn handle_caller(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    call_id: String,
) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut rx = rx;
    let my_gen = state.calls.register_caller(&call_id, tx);

    // Replay the assignment if the agent claimed before this socket connected
    // (or the caller reconnected mid-call) — otherwise the caller would never
    // create the WebRTC offer.
    if let Some(call) = state.calls.get(&call_id) {
        if matches!(call.status, CallStatus::Engaged) {
            if let Some(agent) = &call.agent_id {
                send_caller(&state, &call_id, &serde_json::json!({ "event": "AGENT_ASSIGNED", "call_id": call_id, "agent_id": agent })).await;
            }
        }
    }

    let send_task = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = receiver.next().await {
        let text = match msg {
            Ok(Message::Binary(b)) => {
                // raw 16 kHz mono int16 PCM from the caller's mic — feeds the
                // live emotion analyzer (no-op once the call is claimed/ended)
                crate::emotion::handle_pcm(&state, &call_id, &b);
                continue;
            }
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("action").and_then(|a| a.as_str()) {
            Some("PING") => {
                send_caller(&state, &call_id, &serde_json::json!({ "event": "PONG" })).await;
            }
            Some("IVR_FINISHED") => {
                // Only the first IVR_FINISHED (RINGING -> WAITING) counts;
                // a late duplicate must not relabel an ENGAGED call WAITING.
                if state.calls.set_waiting(&call_id) {
                    // broadcast first (in-memory truth drives the queue), then persist —
                    // a slow/congested DB must never delay the officer queue update
                    if let Some(call) = state.calls.get(&call_id) {
                        broadcast_agents(&state, &serde_json::json!({ "event": "STATUS_CHANGED", "call_id": call_id, "status": "WAITING", "call": call })).await;
                    }
                    db::record_call_status(&state.db, &call_id, None, "WAITING", "IVR_FINISHED").await;
                }
            }
            Some("WEBRTC_SIGNAL") => {
                // Caller path supplies call_id (caller owns exactly one call, but we
                // trust the socket's path id over any client field).
                if let Some(call) = state.calls.get(&call_id) {
                    if let Some(agent_id) = call.agent_id {
                        if let Some(tx) = state.calls.agent_sockets.get(&agent_id) {
                            let _ = tx.send(
                                serde_json::json!({ "event": "WEBRTC_SIGNAL", "call_id": call_id, "payload": v.get("payload") }).to_string(),
                            );
                        }
                    }
                }
            }
            Some("END_CALL") => {
                let _ = state.calls.end(&call_id, None);
                crate::emotion::stop_live(&state, &call_id);
                db::record_call_status(&state.db, &call_id, None, "ENDED", "ENDED_BY_CALLER").await;
                broadcast_agents(&state, &serde_json::json!({ "event": "CALL_ENDED", "call_id": call_id })).await;
                break;
            }
            _ => {}
        }
    }

    send_task.abort();
    // Only the latest caller connection owns the socket slot; if the caller
    // reconnected (auto-reconnect in the client) the generation changed and we
    // leave everything alone. Otherwise give a short grace period, then end
    // the call so a vanished caller tab can't clog the queue forever.
    if state.calls.caller_gen(&call_id) == my_gen {
        state.calls.caller_sockets.remove(&call_id);
        let state2 = state.clone();
        let cid = call_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            if state2.calls.caller_gen(&cid) != my_gen {
                return; // caller reconnected
            }
            if let Some(call) = state2.calls.get(&cid) {
                if !matches!(call.status, CallStatus::Ended) {
                    let agent = call.agent_id.clone();
                    let _ = state2.calls.end(&cid, None);
                    crate::emotion::stop_live(&state2, &cid);
                    db::record_call_status(&state2.db, &cid, agent.as_deref(), "ENDED", "CALLER_DISCONNECTED").await;
                    broadcast_agents(&state2, &serde_json::json!({ "event": "CALL_ENDED", "call_id": cid })).await;
                }
            }
        });
    }
}

/// Agent socket: /ws/agent/{agent_id} (cookie auth)
pub async fn agent_ws(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let authed = auth::agent_from_headers(&state, &headers);
    if authed.as_deref() != Some(agent_id.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // CSWSH guard: a web page must not be able to open this cookie-authenticated
    // socket from the officer's browser. Non-browser clients send no Origin.
    // Behind the platform proxy the raw Host header may differ from the public
    // origin, so also accept X-Forwarded-Host (set by the proxy). Ports stripped.
    if let Some(origin) = headers.get("origin").and_then(|o| o.to_str().ok()) {
        let strip_port = |h: &str| -> String {
            h.split('/').next().unwrap_or(h).split(':').next().unwrap_or(h).to_string()
        };
        let origin_host = strip_port(origin.split("://").nth(1).unwrap_or(""));
        let mut ok = false;
        for name in ["host", "x-forwarded-host"] {
            if let Some(v) = headers.get(name).and_then(|h| h.to_str().ok()) {
                if strip_port(v) == origin_host {
                    ok = true;
                }
            }
        }
        if !ok {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_agent(socket, state, agent_id))
        .into_response()
}

async fn handle_agent(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    agent_id: String,
) {
    use dashmap::mapref::entry::Entry;
    let (mut sender, mut receiver) = socket.split();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let tx = Arc::new(tx);
    let mut rx = rx;

    // kick previous tab (its read loop exits when that socket closes; the exit
    // path below checks ownership before touching the slot, so a quick
    // reconnect can never lose its session or its active call)
    if let Some((_, old)) = state.calls.agent_sockets.remove(&agent_id) {
        let _ = old.send(serde_json::json!({ "event": "DUPLICATE_LOGIN" }).to_string());
    }
    state.calls.agent_sockets.insert(agent_id.clone(), tx.clone());

    // snapshot on connect
    let snapshot = state.calls.snapshot();
    let _ = sender
        .send(Message::Text(
            serde_json::json!({ "event": "QUEUE_SNAPSHOT", "calls": snapshot }).to_string().into(),
        ))
        .await;

    let send_task = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let reply = |msg: serde_json::Value| {
        if let Some(tx) = state.calls.agent_sockets.get(&agent_id) {
            let _ = tx.send(msg.to_string());
        }
    };

    while let Some(msg) = receiver.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let call_id = v.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_string();
        match v.get("action").and_then(|a| a.as_str()) {
            Some("PING") => reply(serde_json::json!({ "event": "PONG" })),
            Some("SYNC_QUEUE") => {
                let snapshot = state.calls.snapshot();
                reply(serde_json::json!({ "event": "QUEUE_SNAPSHOT", "calls": snapshot }));
            }
            Some("CLAIM_CALL") => match state.calls.claim(&call_id, &agent_id) {
                Ok(call) => {
                    // in-call emotions keep streaming after claim and are
                    // persisted (only pre-claim queue readings stay memory-only)
                    let emo = call.emotion.clone();
                    let triage = call.triage.clone();
                    db::record_call_status(&state.db, &call_id, Some(&agent_id), "ENGAGED", "CLAIMED").await;
                    reply(serde_json::json!({ "event": "CLAIM_RESULT", "call_id": call_id, "success": true, "emotion": emo, "triage": triage }));
                    broadcast_agents(&state, &serde_json::json!({ "event": "CALL_CLAIMED", "call_id": call_id, "agent_id": agent_id, "emotion": emo, "triage": triage })).await;
                    send_caller(&state, &call_id, &serde_json::json!({ "event": "AGENT_ASSIGNED", "call_id": call_id, "agent_id": agent_id, "emotion": emo, "triage": triage })).await;
                }
                Err(reason) => reply(serde_json::json!({ "event": "CLAIM_RESULT", "call_id": call_id, "success": false, "reason": reason })),
            },
            Some("RELEASE_CALL") => match state.calls.release(&call_id, &agent_id) {
                Ok(_) => {
                    db::record_call_status(&state.db, &call_id, None, "WAITING", "RELEASED").await;
                    broadcast_agents(&state, &serde_json::json!({ "event": "CALL_RELEASED", "call_id": call_id })).await;
                    send_caller(&state, &call_id, &serde_json::json!({ "event": "CALL_RELEASED", "call_id": call_id })).await;
                }
                Err(reason) => reply(serde_json::json!({ "event": "ERROR", "call_id": call_id, "message": reason })),
            },
            Some("WEBRTC_SIGNAL") => {
                // Agent relays to the caller only if it owns that call.
                // Always include call_id so the caller can route it.
                if let Some(call) = state.calls.get(&call_id) {
                    if call.agent_id.as_deref() == Some(agent_id.as_str()) {
                        send_caller(&state, &call_id, &serde_json::json!({ "event": "WEBRTC_SIGNAL", "call_id": call_id, "payload": v.get("payload") })).await;
                    } else {
                        reply(serde_json::json!({ "event": "ERROR", "call_id": call_id, "message": "not your call" }));
                    }
                }
            }
            Some("END_CALL") => match state.calls.end(&call_id, Some(&agent_id)) {
                Ok(_) => {
                    crate::emotion::stop_live(&state, &call_id);
                    db::record_call_status(&state.db, &call_id, Some(&agent_id), "ENDED", "ENDED_BY_AGENT").await;
                    broadcast_agents(&state, &serde_json::json!({ "event": "CALL_ENDED", "call_id": call_id })).await;
                    send_caller(&state, &call_id, &serde_json::json!({ "event": "CALL_ENDED", "call_id": call_id })).await;
                }
                Err(reason) => reply(serde_json::json!({ "event": "ERROR", "call_id": call_id, "message": reason })),
            },
            _ => {}
        }
    }

    send_task.abort();
    // Only release calls / drop the slot if we still own it — a reconnected
    // tab registered a fresh tx and must not lose its session (or its call).
    let still_owner = match state.calls.agent_sockets.entry(agent_id.clone()) {
        Entry::Occupied(e) if Arc::ptr_eq(e.get(), &tx) => {
            e.remove();
            true
        }
        _ => false,
    };
    if !still_owner {
        return;
    }
    // release any engaged calls so callers aren't stranded
    let owned: Vec<String> = state
        .calls
        .snapshot()
        .into_iter()
        .filter(|c: &Call| c.agent_id.as_deref() == Some(agent_id.as_str()))
        .map(|c| c.call_id)
        .collect();
    for id in owned {
        if state.calls.release(&id, &agent_id).is_ok() {
            db::record_call_status(&state.db, &id, Some(&agent_id), "WAITING", "AGENT_DISCONNECTED").await;
        }
        broadcast_agents(&state, &serde_json::json!({ "event": "CALL_RELEASED", "call_id": id })).await;
        send_caller(&state, &id, &serde_json::json!({ "event": "CALL_RELEASED", "call_id": id })).await;
    }
}
