//! Minimal Microsoft Graph client (blocking; call from worker threads only).
use crate::auth;
use serde_json::Value;
use std::time::Duration;

pub const BASE: &str = "https://graph.microsoft.com/v1.0";

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn finish(mut r: ureq::http::Response<ureq::Body>) -> Result<Value, String> {
    let code = r.status().as_u16();
    let v: Value = r.body_mut().read_json().unwrap_or(Value::Null);
    if code >= 400 {
        let m = v["error"]["message"].as_str().unwrap_or("request failed");
        return Err(format!("Graph {code}: {m}"));
    }
    Ok(v)
}

pub fn get(path: &str) -> Result<Value, String> {
    get_h(path, &[])
}

/// GET with extra headers, e.g. ("Prefer", "outlook.body-content-type=\"text\"").
pub fn get_h(path: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    let mut req = agent()
        .get(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    finish(req.call().map_err(|e| e.to_string())?)
}

pub fn post(path: &str, body: &Value) -> Result<Value, String> {
    let r = agent()
        .post(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?))
        .send_json(body)
        .map_err(|e| e.to_string())?;
    finish(r)
}

/// Percent-encode a query value.
pub fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}
