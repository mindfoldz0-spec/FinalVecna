use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const AGENT_COOKIE: &str = "vecna_agent";
pub const CALL_TOKEN_HEADER: &str = "x-call-token";

/// Call tokens expire (24h) so a leaked token can't reconnect forever.
pub const CALL_TOKEN_TTL_SECS: u64 = 24 * 3600;

pub fn sign_call_token(secret: &str, call_id: &str) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + CALL_TOKEN_TTL_SECS;
    let msg = format!("{call_id}.{exp}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    format!("{call_id}.{exp}.{}", hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_call_token(secret: &str, call_id: &str, token: &str) -> bool {
    let mut parts = token.splitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(cid), Some(exp_s), Some(sig)) if cid == call_id => {
            let exp: u64 = match exp_s.parse() {
                Ok(e) => e,
                Err(_) => return false,
            };
            let msg = format!("{call_id}.{exp}");
            let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
                Ok(m) => m,
                Err(_) => return false,
            };
            mac.update(msg.as_bytes());
            let expected = hex::encode(mac.finalize().into_bytes());
            use subtle::ConstantTimeEq;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            expected.as_bytes().ct_eq(sig.as_bytes()).into() && exp >= now
        }
        _ => false,
    }
}

pub fn sign_agent_session(secret: &str, agent_id: &str, exp: u64) -> String {
    let msg = format!("{agent_id}.{exp}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    format!("{}.{}.{}", agent_id, exp, hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_agent_session(secret: &str, cookie: &str) -> Option<String> {
    let mut parts = cookie.splitn(3, '.');
    let agent_id = parts.next()?;
    let exp: u64 = parts.next()?.parse().ok()?;
    let sig = parts.next()?;
    let msg = format!("{agent_id}.{exp}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    let ok: bool = {
        use subtle::ConstantTimeEq;
        expected.as_bytes().ct_eq(sig.as_bytes()).into()
    };
    if !ok {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if exp < now {
        return None;
    }
    Some(agent_id.to_string())
}

/// Three named officers share one password (per user request).
pub const AGENTS: [&str; 3] = ["mayuresh_super", "tejas_dimitri", "Aman_clasp"];

pub async fn list_agents() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "agents": AGENTS }))
}

#[derive(Deserialize)]
pub struct AgentLogin {
    pub agent_id: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct AgentSession {
    pub agent_id: String,
    pub expires_in: u64,
}

pub async fn agent_login(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(body): Json<AgentLogin>,
) -> Result<(HeaderMap, Json<AgentSession>), StatusCode> {
    crate::rate_check("login", addr.ip())?;
    let agent_id = body.agent_id.trim();
    if agent_id.is_empty() || agent_id.len() > 64 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !AGENTS.contains(&agent_id) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let ok: bool = {
        use subtle::ConstantTimeEq;
        if state.agent_password.len() != body.password.len() {
            false
        } else {
            state.agent_password.as_bytes().ct_eq(body.password.as_bytes()).into()
        }
    };
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    crate::db::touch_agent_login(&state.db, agent_id).await;
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 8 * 3600;
    let session = sign_agent_session(&state.call_token_secret, agent_id, exp);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        format!("{AGENT_COOKIE}={session}; HttpOnly; SameSite=Lax; Path=/; Max-Age=28800")
            .parse()
            .unwrap(),
    );
    Ok((headers, Json(AgentSession { agent_id: agent_id.to_string(), expires_in: 28800 })))
}

pub async fn agent_me(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<AgentSession>, StatusCode> {
    let agent_id = agent_from_headers(&state, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Json(AgentSession { agent_id, expires_in: 28800 }))
}

pub fn agent_from_headers(state: &crate::AppState, headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{AGENT_COOKIE}=")) {
            return verify_agent_session(&state.call_token_secret, v);
        }
    }
    None
}

