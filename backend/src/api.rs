use axum::{
    extract::{Path, State},
    Json,
};

use crate::AppState;

/// GET /api/calls — recent calls with recording counts (officer dashboard / analysis UI).
/// Served from the in-memory mirrors (DynamoDB is written fire-and-forget).
pub async fn list_calls(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut calls: Vec<serde_json::Value> = state.db.calls.iter().map(|c| c.value().clone()).collect();
    calls.sort_by(|a, b| {
        let ea = a.get("created_epoch").and_then(|v| v.as_i64()).unwrap_or(0);
        let eb = b.get("created_epoch").and_then(|v| v.as_i64()).unwrap_or(0);
        eb.cmp(&ea)
    });
    calls.truncate(200);
    let out: Vec<serde_json::Value> = calls
        .iter()
        .map(|c| {
            let call_id = c.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let recordings = state
                .db
                .recordings
                .iter()
                .filter(|r| r.get("call_id").and_then(|v| v.as_str()) == Some(call_id))
                .count() as i64;
            let (last_emotion, last_urgency) = state
                .db
                .emotions
                .get(call_id)
                .and_then(|v| v.last().cloned())
                .map(|e| {
                    (
                        e.get("label").and_then(|v| v.as_str()).map(String::from),
                        e.get("urgency").and_then(|v| v.as_i64()),
                    )
                })
                .unwrap_or((None, None));
            let mut triage_rows: Vec<serde_json::Value> = state
                .db
                .analyses
                .iter()
                .filter(|a| {
                    a.get("call_id").and_then(|v| v.as_str()) == Some(call_id)
                        && a.get("kind").and_then(|v| v.as_str()) == Some("triage")
                })
                .map(|a| a.clone())
                .collect();
            triage_rows.sort_by_key(|a| a.get("created_epoch").and_then(|v| v.as_i64()).unwrap_or(0));
            let last_triage = triage_rows.last().and_then(|a| {
                a.get("result_json")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            });
            serde_json::json!({
                "call_id": c.get("call_id"),
                "dialed_number": c.get("dialed_number"),
                "status": c.get("status"),
                "agent_id": c.get("agent_id"),
                "caller_ip": c.get("caller_ip"),
                "created_at": c.get("created_at"),
                "claimed_at": c.get("claimed_at"),
                "engaged_at": c.get("engaged_at"),
                "ended_at": c.get("ended_at"),
                "ivr_finished_at": c.get("ivr_finished_at"),
                "recordings": recordings,
                "last_emotion": last_emotion,
                "last_urgency": last_urgency,
                "last_category": last_triage.as_ref().and_then(|v| v.get("category").and_then(|x| x.as_str()).map(String::from)),
                "last_triage_urgency": last_triage.as_ref().and_then(|v| v.get("urgency").and_then(|x| x.as_i64())),
            })
        })
        .collect();
    Json(serde_json::json!({ "calls": out }))
}

/// GET /api/calls/{id} — full detail: call + recordings + transcripts + analyses + events.
pub async fn call_detail(State(state): State<AppState>, Path(call_id): Path<String>) -> Json<serde_json::Value> {
    let call = state.db.calls.get(&call_id).map(|c| c.value().clone());
    let recordings: Vec<serde_json::Value> = state
        .db
        .recordings
        .iter()
        .filter(|r| r.get("call_id").and_then(|v| v.as_str()) == Some(call_id.as_str()))
        .map(|r| {
            let v = r.value();
            serde_json::json!({
                "recording_id": v.get("recording_id"),
                "call_id": v.get("call_id"),
                "side": v.get("side"),
                "filename": v.get("filename"),
                "bytes": v.get("bytes"),
                "mime": v.get("mime"),
                "duration_ms": v.get("duration_ms"),
                "rms": v.get("rms"),
                "zcr": v.get("zcr"),
                "storage": v.get("storage"),
                "s3_key": v.get("s3_key"),
                "created_at": v.get("created_at"),
            })
        })
        .collect();
    let transcripts: Vec<serde_json::Value> = state
        .db
        .transcripts
        .iter()
        .filter(|t| t.get("call_id").and_then(|v| v.as_str()) == Some(call_id.as_str()))
        .map(|t| {
            let v = t.value();
            serde_json::json!({
                "transcript_id": v.get("transcript_id"),
                "recording_id": v.get("recording_id"),
                "call_id": v.get("call_id"),
                "language": v.get("language"),
                "text": v.get("text"),
                "confidence": v.get("confidence"),
                "provider": v.get("provider"),
                "created_at": v.get("created_at"),
            })
        })
        .collect();
    let analyses: Vec<serde_json::Value> = state
        .db
        .analyses
        .iter()
        .filter(|a| a.get("call_id").and_then(|v| v.as_str()) == Some(call_id.as_str()))
        .map(|a| {
            let v = a.value();
            serde_json::json!({
                "analysis_id": v.get("analysis_id"),
                "recording_id": v.get("recording_id"),
                "call_id": v.get("call_id"),
                "kind": v.get("kind"),
                "provider": v.get("provider"),
                "model": v.get("model"),
                "result_json": v.get("result_json"),
                "created_at": v.get("created_at"),
            })
        })
        .collect();
    let events: Vec<serde_json::Value> = state
        .db
        .events
        .get(&call_id)
        .map(|v| {
            v.iter()
                .map(|e| {
                    serde_json::json!({
                        "event": e.get("event"),
                        "agent_id": e.get("agent_id"),
                        "detail_json": e.get("detail_json"),
                        "created_at": e.get("created_at"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let emotions: Vec<serde_json::Value> = state
        .db
        .emotions
        .get(&call_id)
        .map(|v| {
            v.iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.get("emotion_id"),
                        "emotion_id": e.get("emotion_id"),
                        "label": e.get("label"),
                        "confidence": e.get("confidence"),
                        "scores_json": e.get("scores_json"),
                        "scores": e.get("scores"),
                        "urgency": e.get("urgency"),
                        "source": e.get("source"),
                        "recording_id": e.get("recording_id"),
                        "created_at": e.get("created_at"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let call_json = call.map(|c| {
        serde_json::json!({
            "call_id": c.get("call_id"),
            "dialed_number": c.get("dialed_number"),
            "status": c.get("status"),
            "agent_id": c.get("agent_id"),
            "created_at": c.get("created_at"),
            "ended_at": c.get("ended_at"),
        })
    });

    Json(serde_json::json!({
        "call": call_json,
        "recordings": recordings,
        "transcripts": transcripts,
        "analyses": analyses,
        "events": events,
        "emotions": emotions,
    }))
}
