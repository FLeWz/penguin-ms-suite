//! OAuth2 device-code flow against the Microsoft identity platform.
//! Public client: no secret. Refresh token stored 0600 in the XDG config dir.
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const SCOPES: &str = "offline_access User.Read Mail.Read Calendars.Read Chat.ReadWrite Files.Read";

static ACCESS: Mutex<Option<(String, Instant)>> = Mutex::new(None);

pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("penguin-ms-suite")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// (client_id, tenant). Env vars override config.json.
pub fn config() -> Result<(String, String), String> {
    let file: Value = fs::read_to_string(dir().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let pick = |env: &str, key: &str| {
        std::env::var(env)
            .ok()
            .or_else(|| file[key].as_str().map(String::from))
    };
    let id = pick("PENGUIN_MS_CLIENT_ID", "client_id").ok_or_else(|| {
        format!(
            "No client_id. Set PENGUIN_MS_CLIENT_ID or write {{\"client_id\":\"...\"}} to {}",
            dir().join("config.json").display()
        )
    })?;
    let tenant = pick("PENGUIN_MS_TENANT", "tenant").unwrap_or_else(|| "common".into());
    Ok((id, tenant))
}

fn endpoint(tenant: &str, leaf: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/{leaf}")
}

fn err_of(v: &Value) -> String {
    v["error_description"]
        .as_str()
        .or(v["error"].as_str())
        .unwrap_or("unknown auth error")
        .to_string()
}

pub fn device_start() -> Result<DeviceCode, String> {
    let (id, tenant) = config()?;
    let mut r = agent()
        .post(endpoint(&tenant, "devicecode"))
        .send_form([("client_id", id.as_str()), ("scope", SCOPES)])
        .map_err(|e| e.to_string())?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    let s = |k: &str| v[k].as_str().map(String::from);
    Ok(DeviceCode {
        device_code: s("device_code").ok_or_else(|| err_of(&v))?,
        user_code: s("user_code").ok_or_else(|| err_of(&v))?,
        uri: s("verification_uri").unwrap_or_default(),
        interval: v["interval"].as_u64().unwrap_or(5),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
    })
}

/// Blocks until the user finishes sign-in (or timeout / denial).
pub fn device_poll(dc: &DeviceCode) -> Result<(), String> {
    let (id, tenant) = config()?;
    let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let mut r = agent()
            .post(endpoint(&tenant, "token"))
            .send_form([
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", id.as_str()),
                ("device_code", dc.device_code.as_str()),
            ])
            .map_err(|e| e.to_string())?;
        let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
        match v["error"].as_str() {
            None => return store(&v),
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(_) => return Err(err_of(&v)),
        }
    }
    Err("Sign-in timed out".into())
}

fn store(v: &Value) -> Result<(), String> {
    let at = v["access_token"].as_str().ok_or("no access_token")?;
    let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
    *ACCESS.lock().unwrap() = Some((at.into(), Instant::now() + Duration::from_secs(ttl)));
    if let Some(rt) = v["refresh_token"].as_str() {
        let d = dir();
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        let p = d.join("refresh_token");
        fs::write(&p, rt).map_err(|e| e.to_string())?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn has_session() -> bool {
    dir().join("refresh_token").exists()
}

pub fn sign_out() {
    *ACCESS.lock().unwrap() = None;
    let _ = fs::remove_file(dir().join("refresh_token"));
}

/// Valid access token; refreshes transparently.
pub fn token() -> Result<String, String> {
    if let Some((t, exp)) = ACCESS.lock().unwrap().as_ref() {
        if Instant::now() < *exp {
            return Ok(t.clone());
        }
    }
    let rt = fs::read_to_string(dir().join("refresh_token")).map_err(|_| "Not signed in")?;
    let (id, tenant) = config()?;
    let mut r = agent()
        .post(endpoint(&tenant, "token"))
        .send_form([
            ("grant_type", "refresh_token"),
            ("client_id", id.as_str()),
            ("refresh_token", rt.trim()),
            ("scope", SCOPES),
        ])
        .map_err(|e| e.to_string())?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    if v["error"].is_string() {
        return Err(err_of(&v));
    }
    store(&v)?;
    Ok(v["access_token"].as_str().unwrap_or_default().to_string())
}
