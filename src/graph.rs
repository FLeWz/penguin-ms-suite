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

pub fn patch(path: &str, body: &Value) -> Result<Value, String> {
    let r = agent()
        .patch(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?))
        .send_json(body)
        .map_err(|e| e.to_string())?;
    finish(r)
}

pub fn delete(path: &str) -> Result<(), String> {
    let r = agent()
        .delete(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?))
        .call()
        .map_err(|e| e.to_string())?;
    finish(r).map(|_| ())
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

fn bytes_agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(secs)))
        .build()
        .into()
}

fn authed_get(path: &str, secs: u64) -> Result<ureq::http::Response<ureq::Body>, String> {
    let r = bytes_agent(secs)
        .get(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?))
        .call()
        .map_err(|e| e.to_string())?;
    let code = r.status().as_u16();
    if code >= 400 {
        return Err(format!("Graph {code}"));
    }
    Ok(r)
}

/// Binary GET (images). Redirects to pre-authenticated download URLs are followed.
pub fn get_bytes(path: &str, max: u64) -> Result<Vec<u8>, String> {
    let r = authed_get(path, 60)?;
    r.into_body()
        .into_with_config()
        .limit(max)
        .read_to_vec()
        .map_err(|e| e.to_string())
}

/// Stream a (possibly large) file to `dest` via a .part file; refuses more than `max` bytes.
pub fn download(path: &str, dest: &std::path::Path, max: u64) -> Result<(), String> {
    let r = authed_get(path, 1800)?;
    let part = dest.with_extension("part");
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut rd = r.into_body().into_with_config().limit(max).reader();
    let res = std::io::copy(&mut rd, &mut f).map_err(|e| e.to_string());
    drop(f);
    match res {
        Ok(_) => std::fs::rename(&part, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

fn put_with(path: &str, send: impl FnOnce(ureq::RequestBuilder<ureq::typestate::WithBody>) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
    let req = bytes_agent(1800)
        .put(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {}", auth::token()?))
        .header("Content-Type", "application/octet-stream");
    finish(send(req).map_err(|e| e.to_string())?)
}

/// Simple upload of a local file (Graph allows up to 250 MB this way).
pub fn put_file(path: &str, file: &std::path::Path) -> Result<Value, String> {
    let f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    put_with(path, |r| r.send(f))
}

pub fn put_bytes(path: &str, bytes: Vec<u8>) -> Result<Value, String> {
    put_with(path, |r| r.send(bytes))
}
